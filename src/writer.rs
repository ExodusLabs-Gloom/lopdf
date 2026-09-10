use std::fs::File;
use std::io::{BufWriter, Result, Write};
use std::path::Path;
use std::vec;

use super::Object::*;
use super::{Dictionary, Document, Object, Stream, StringFormat};
use crate::encryption;
use crate::{IncrementalDocument, xref::*};

impl Document {
    /// Save PDF document to specified file path.
    #[inline]
    pub fn save<P: AsRef<Path>>(&mut self, path: P) -> Result<File> {
        let mut file = BufWriter::new(File::create(path)?);
        self.save_internal(&mut file)?;
        Ok(file.into_inner()?)
    }

    /// Save PDF to arbitrary target
    #[inline]
    pub fn save_to<W: Write>(&mut self, target: &mut W) -> Result<()> {
        self.save_internal(target)
    }

    /// Save PDF with custom options
    ///
    /// Object streams are skipped for an encrypted document, which is written with every
    /// object serialized individually instead. See [`Document::save_modern`].
    ///
    /// Object streams need cross-reference authority able to encode type-2 compressed
    /// entries, which lopdf provides through cross-reference streams. The hybrid-reference
    /// file that could carry them next to a classic cross-reference table is not
    /// implemented, so a save that would emit object streams while keeping a classic
    /// cross-reference table fails with [`std::io::ErrorKind::Unsupported`] before any
    /// byte is written and before the document is modified. A configuration that cannot
    /// hold any object (zero `ObjectStreamConfig::max_objects_per_stream`) is rejected
    /// with [`std::io::ErrorKind::InvalidInput`] whenever an object stream would actually
    /// be built; the builder never produces it, but the configuration fields are public.
    pub fn save_with_options<W: Write>(&mut self, target: &mut W, options: crate::SaveOptions) -> Result<()> {
        use crate::ObjectStream;

        // Preflight, before the version, the reference table, the trailer, the objects or
        // any output byte are touched. A live object packed into an object stream can only
        // be located through a type-2 compressed cross-reference entry, which a classic
        // table cannot carry. `use_xref_streams = false` preserves the document's current
        // representation instead of forcing a classic table, so a document that already
        // uses cross-reference streams keeps working with object streams enabled.
        let selected_xref_type = if options.use_xref_streams {
            XrefType::CrossReferenceStream
        } else {
            self.reference_table.cross_reference_type
        };

        let has_object_stream_candidates = options.use_object_streams
            && !self.is_encrypted()
            && self.objects.iter().any(|(&(id, generation), object)| {
                generation == 0 && ObjectStream::can_be_compressed((id, generation), object, self)
            });

        if has_object_stream_candidates {
            // Only configuration that would actually be used is validated here: an
            // encrypted document skips object streams entirely, and with no eligible
            // object no object stream is constructed, so capacity stays irrelevant.
            if options.object_stream_config.max_objects_per_stream == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "object stream capacity must be greater than zero",
                ));
            }
            if matches!(selected_xref_type, XrefType::CrossReferenceTable) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "object streams require cross-reference authority capable of type-2 compressed entries; \
                     hybrid-reference output is not implemented",
                ));
            }
        }

        // Cross-reference streams are independent of object streams: a document can use one
        // without the other. Select the requested type here so the choice applies whichever
        // path writes the body below. Both features arrived in PDF 1.5, so a document that is
        // about to use one is moved up to that version.
        if options.use_xref_streams {
            self.reference_table.cross_reference_type = XrefType::CrossReferenceStream;

            if self.version.as_str() < "1.5" {
                self.version = "1.5".to_string();
            }
        }

        if options.use_object_streams {
            self.save_with_object_streams(target, options)
        } else {
            self.save_internal(target)
        }
    }

    /// Save PDF with modern features (object streams and cross-reference streams)
    ///
    /// An encrypted document is written without object streams. The objects are already
    /// encrypted by the time they reach the writer and the file encryption key is gone,
    /// so an object stream built here could only be written in the clear, contradicting
    /// the document's `/Encrypt` dictionary. The requested cross-reference type is still
    /// used; cross-reference streams are never encrypted.
    pub fn save_modern<W: Write>(&mut self, target: &mut W) -> Result<()> {
        let options = crate::SaveOptions {
            use_object_streams: true,
            use_xref_streams: true,
            ..Default::default()
        };
        self.save_with_options(target, options)
    }

    fn save_internal<W: Write>(&mut self, target: &mut W) -> Result<()> {
        let mut target = CountingWrite {
            inner: target,
            bytes_written: 0,
        };

        let mut xref = Xref::new(self.max_id + 1, self.reference_table.cross_reference_type);
        writeln!(target, "%PDF-{}", self.version)?;

        Writer::write_binary_mark(&mut target, &self.binary_mark)?;

        for (&(id, generation), object) in &self.objects {
            if object
                .type_name()
                .map(|name| [b"ObjStm".as_slice(), b"XRef".as_slice(), b"Linearized".as_slice()].contains(&name))
                .ok()
                != Some(true)
            {
                Writer::write_indirect_object(&mut target, id, generation, object, &mut xref)?;
            }
        }

        let xref_start = target.bytes_written;

        // Pick right cross reference stream.
        match xref.cross_reference_type {
            XrefType::CrossReferenceTable => {
                self.normalize_full_save_xref(&mut xref);
                Writer::write_xref(&mut target, &xref)?;
                self.write_trailer(&mut target, xref.size)?;
            }
            XrefType::CrossReferenceStream => {
                // Cross Reference Stream instead of XRef and Trailer
                self.write_cross_reference_stream(&mut target, &mut xref, true)?;
            }
        }
        // Write `startxref` part of trailer
        write!(target, "\nstartxref\n{xref_start}\n%%EOF")?;

        Ok(())
    }

    /// Save PDF with object streams enabled
    fn save_with_object_streams<W: Write>(&mut self, target: &mut W, options: crate::SaveOptions) -> Result<()> {
        use crate::ObjectStream;
        use std::collections::HashMap;

        // Ensure PDF version is at least 1.5 (required for object streams)
        if self.version.as_str() < "1.5" {
            self.version = "1.5".to_string();
        }

        // Object streams are built here, while serializing, but the document's objects were
        // already encrypted by `Document::encrypt`, which drops the file encryption key once
        // it is done. There is nothing left to encrypt a new stream with, so it would go out
        // in the clear while the `/Encrypt` dictionary claims every stream is encrypted, and
        // the strings packed into it would stay encrypted a second time: an object stream is
        // itself the unit of encryption, and strings inside one shall not be encrypted
        // separately. Write the objects out individually instead, as `save` does. The
        // cross-reference type chosen in `save_with_options` still applies.
        if self.is_encrypted() {
            return self.save_internal(target);
        }

        let mut target = CountingWrite {
            inner: target,
            bytes_written: 0,
        };

        let mut xref = Xref::new(self.max_id + 1, self.reference_table.cross_reference_type);
        writeln!(target, "%PDF-{}", self.version)?;
        Writer::write_binary_mark(&mut target, &self.binary_mark)?;

        // Organize objects into streams
        // Generated type-2 member indices must fit lopdf's u16 representation.
        const MAX_GENERATED_OBJECT_STREAM_MEMBERS: usize = u16::MAX as usize + 1;
        let effective_max = options
            .object_stream_config
            .max_objects_per_stream
            .min(MAX_GENERATED_OBJECT_STREAM_MEMBERS);
        let mut object_streams: Vec<crate::ObjectStream> = Vec::new();
        let mut objects_to_write_directly = Vec::new();
        let mut object_to_stream_map = HashMap::new();

        // Categorize objects
        for (&(id, generation), object) in &self.objects {
            // Skip existing object streams - we'll create new ones
            if let Object::Stream(stream) = object
                && let Ok(type_obj) = stream.dict.get(b"Type")
                && let Ok(type_name) = type_obj.as_name()
                && type_name == b"ObjStm"
            {
                continue; // Skip existing object streams
            }

            if generation == 0 && ObjectStream::can_be_compressed((id, generation), object, self) {
                // Object can be compressed
                // Find or create an object stream for it
                let stream_index = object_streams.len().saturating_sub(1);

                if object_streams.is_empty() || object_streams[stream_index].object_count() >= effective_max {
                    // Create new object stream
                    let new_stream = ObjectStream::builder()
                        .max_objects(effective_max)
                        .compression_level(options.object_stream_config.compression_level)
                        .build();
                    object_streams.push(new_stream);
                }

                let stream_index = object_streams.len() - 1;
                // The object has already been pulled out of direct serialization, so a
                // failed insertion would silently drop a live object from the file. The
                // save must fail instead.
                object_streams[stream_index]
                    .add_object((id, generation), object.clone())
                    .map_err(std::io::Error::other)?;
                object_to_stream_map.insert((id, generation), stream_index);
            } else {
                // Object must be written directly
                objects_to_write_directly.push(((id, generation), object));
            }
        }

        // Write direct objects first
        for ((id, generation), object) in objects_to_write_directly {
            Writer::write_indirect_object(&mut target, id, generation, object, &mut xref)?;
        }

        // Write object streams
        let mut stream_count = 0;
        for obj_stream in object_streams.into_iter() {
            let stream_id = self.max_id + 1 + stream_count;
            let stream_obj = obj_stream.to_stream_object().map_err(std::io::Error::other)?;

            // Record compressed objects in xref
            // Must use the same sort order as build_stream_content()
            let mut sorted_objects: Vec<_> = obj_stream.objects.keys().cloned().collect();
            sorted_objects.sort_by_key(|id| *id);
            for (index_in_stream, (obj_id, _gen)) in sorted_objects.iter().enumerate() {
                xref.insert(
                    *obj_id,
                    XrefEntry::Compressed {
                        container: stream_id,
                        index: u16::try_from(index_in_stream).map_err(std::io::Error::other)?,
                    },
                );
            }

            // Write the object stream
            Writer::write_indirect_object(&mut target, stream_id, 0, &Object::Stream(stream_obj), &mut xref)?;
            stream_count += 1;
        }

        // Update max_id to account for object streams
        self.max_id += stream_count;

        let xref_start = target.bytes_written;

        // Write cross-reference
        match xref.cross_reference_type {
            XrefType::CrossReferenceTable => {
                self.normalize_full_save_xref(&mut xref);
                Writer::write_xref(&mut target, &xref)?;
                self.write_trailer(&mut target, xref.size)?;
            }
            XrefType::CrossReferenceStream => {
                self.write_cross_reference_stream(&mut target, &mut xref, true)?;
            }
        }

        write!(target, "\nstartxref\n{xref_start}\n%%EOF")?;
        Ok(())
    }

    /// Rebuild a full rewrite's free list from effective identities and generations.
    fn normalize_full_save_xref(&self, xref: &mut Xref) {
        // Preserve allocation state, emitted IDs, and explicit effective Free IDs.
        // Neither the input trailer's Size nor reference_table.size is output capacity.
        let highest_free = self
            .reference_table
            .entries
            .iter()
            .filter_map(|(&id, entry)| matches!(entry, XrefEntry::Free { .. } | XrefEntry::UnusableFree).then_some(id))
            .max()
            .unwrap_or(0);
        let output_max = self.max_id.max(xref.max_id()).max(highest_free);
        let mut next_free = 0;
        // Walking backwards builds ascending links without trusting source pointers.
        for id in (1..=output_max).rev() {
            if matches!(
                xref.get(id),
                Some(XrefEntry::Normal { .. } | XrefEntry::Compressed { .. })
            ) {
                continue;
            }
            let generation = match self.reference_table.get(id) {
                Some(XrefEntry::Free { generation, .. }) => *generation,
                Some(XrefEntry::UnusableFree | XrefEntry::Null) => u16::MAX,
                Some(XrefEntry::Normal { generation, .. }) => generation.saturating_add(1),
                Some(XrefEntry::Compressed { .. }) => 1,
                None => 0,
            };
            let reusable = generation < u16::MAX;
            xref.insert(
                id,
                XrefEntry::Free {
                    next_free: if reusable { next_free } else { 0 },
                    generation,
                },
            );
            if reusable {
                next_free = id;
            }
        }
        xref.insert(
            0,
            XrefEntry::Free {
                next_free,
                generation: u16::MAX,
            },
        );
        xref.size = output_max + 1;
    }

    /// Write the Cross Reference Stream.
    ///
    /// Insert an `Object` to the end of the PDF (not visible when inspecting `Document`).
    /// Note: This is different from the "Cross Reference Table".
    fn write_cross_reference_stream<W: Write>(
        &mut self, file: &mut CountingWrite<&mut W>, xref: &mut Xref, full_save: bool,
    ) -> Result<()> {
        let xref_start = file.checked_position_u32()?;
        // Increment max_id to account for CRS.
        self.max_id += 1;
        let new_obj_id_for_crs = self.max_id;
        xref.insert(
            new_obj_id_for_crs,
            XrefEntry::Normal {
                offset: xref_start,
                generation: 0,
            },
        );
        if full_save {
            self.normalize_full_save_xref(xref);
        }
        self.trailer.set("Type", Name(b"XRef".to_vec()));
        // Update `max_id` in trailer
        self.trailer
            .set("Size", i64::from(if full_save { xref.size } else { self.max_id + 1 }));
        // Set the size of each entry in bytes (default for PDFs is `[1 2 1]`)
        // In our case we use `[u8, u32, u16]` for each entry
        // to keep things simple and working at all times.
        self.trailer.set("W", Array(vec![Integer(1), Integer(4), Integer(2)]));
        // Note that `ASCIIHexDecode` does not work correctly,
        // but is still useful for debugging sometimes.
        let filter = XRefStreamFilter::None;
        let (stream, stream_length, indexes) = Writer::create_xref_steam(xref, filter)?;
        self.trailer.set("Index", indexes);

        if filter == XRefStreamFilter::ASCIIHexDecode {
            self.trailer.set("Filter", Name(b"ASCIIHexDecode".to_vec()));
        } else {
            self.trailer.remove(b"Filter");
        }

        self.trailer.set("Length", stream_length as i64);

        let trailer = &self.trailer;
        let cross_reference_stream = Stream(Stream {
            dict: trailer.clone(),
            allows_compression: true,
            content: stream,
            start_position: None,
        });
        // Insert Cross Reference Stream as an `Object` to the end of the PDF.
        // The `Object` is not added to `Document` because it is generated every time you save.
        Writer::write_indirect_object(file, new_obj_id_for_crs, 0, &cross_reference_stream, xref)?;

        Ok(())
    }

    fn write_trailer(&mut self, file: &mut dyn Write, size: u32) -> Result<()> {
        self.trailer.set("Size", i64::from(size));
        file.write_all(b"trailer\n")?;
        Writer::write_dictionary(file, &self.trailer)?;
        Ok(())
    }
}

impl IncrementalDocument {
    /// Save PDF document to specified file path.
    ///
    /// The `check_incremental_save_supported` guard is invoked before
    /// `File::create` so an unsupported input (e.g. a still-encrypted
    /// previous revision) does not truncate a pre-existing file at `path`.
    #[inline]
    pub fn save<P: AsRef<Path>>(&mut self, path: P) -> Result<File> {
        self.check_incremental_save_supported()?;
        let mut file = BufWriter::new(File::create(path)?);
        self.save_internal(&mut file)?;
        Ok(file.into_inner()?)
    }

    /// Save PDF to arbitrary target
    #[inline]
    pub fn save_to<W: Write>(&mut self, target: &mut W) -> Result<()> {
        self.save_internal(target)
    }

    /// Reject the two cases we still cannot handle: a document that arrived
    /// still-encrypted (no password was supplied), and the inconsistent case
    /// of `encryption_state` set but `encrypt_object_id` missing (which should
    /// not occur in practice — `decrypt_raw` records the id — but we guard
    /// against it defensively).
    fn check_incremental_save_supported(&self) -> Result<()> {
        let prev = self.get_prev_documents();
        if prev.is_encrypted() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "incremental update of a still-encrypted PDF is not supported: \
                 call `Document::decrypt` on the previous revision first \
                 (see https://github.com/J-F-Liu/lopdf/issues/520)",
            ));
        }
        if let Some(state) = prev.encryption_state.as_ref()
            && state.encrypt_object_id().is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "cannot incrementally save this decrypted document: \
                 the /Encrypt object id was not recorded during decryption",
            ));
        }
        Ok(())
    }

    fn save_internal<W: Write>(&mut self, target: &mut W) -> Result<()> {
        self.check_incremental_save_supported()?;

        // If the previous revision was encrypted (and successfully decrypted),
        // we need to re-encrypt every appended object with the same encryption
        // state and restore the trailer's `/Encrypt` reference.
        //
        // Cloning `EncryptionState` (small, mostly `Vec<u8>`) avoids borrow
        // conflicts between `&self.prev_documents` and `&mut self.new_document`.
        let encryption_state = self.get_prev_documents().encryption_state.as_ref().cloned();

        let mut target = CountingWrite {
            inner: target,
            bytes_written: 0,
        };

        // Write previous document versions.
        let prev_document_bytes = self.get_prev_documents_bytes();
        target.write_all(prev_document_bytes)?;

        // Write/Append new document version.
        let mut xref = Xref::new(
            self.new_document.max_id + 1,
            self.get_prev_documents().reference_table.cross_reference_type,
        );

        if let Some(last_byte) = prev_document_bytes.last()
            && *last_byte != b'\n'
        {
            // Add a newline if it was not already present
            writeln!(target)?;
        }

        // No file header and no binary marker here. An incremental update is
        // defined (ISO 32000-1, 7.5.6) as the original file followed by the
        // changed objects, a cross-reference section and a trailer — the
        // header belongs to the file, which the previous revision already
        // carries. Emitting a second "%PDF-x.y" makes the appended region
        // look like the start of another document to anything that locates a
        // PDF by scanning for the header, and because `new_document`'s
        // version defaults to 1.4 it also understated the format of every
        // file built on a later version.

        // Write each newly added indirect object. When the document is
        // encrypted, each object is cloned first and the clone is encrypted;
        // the in-memory objects are left as plaintext so that further edits
        // and repeated saves do not double-encrypt.
        for (&(id, generation), object) in &self.new_document.objects {
            if object
                .type_name()
                .map(|name| [b"ObjStm".as_slice(), b"XRef".as_slice(), b"Linearized".as_slice()].contains(&name))
                .ok()
                != Some(true)
            {
                if let Some(state) = encryption_state.as_ref() {
                    let mut encrypted = object.clone();
                    encryption::encrypt_object(state, (id, generation), &mut encrypted)
                        .map_err(std::io::Error::other)?;
                    Writer::write_indirect_object(&mut target, id, generation, &encrypted, &mut xref)?;
                } else {
                    Writer::write_indirect_object(&mut target, id, generation, object, &mut xref)?;
                }
            }
        }

        // For an encrypted document, install a modified copy of the trailer
        // that restores the /Encrypt reference. We swap it in temporarily so
        // that `write_trailer` / `write_cross_reference_stream` — which
        // already mutate the trailer to update Size/W/Length/etc — operate
        // on the modified copy, and swap it back afterwards so that the
        // in-memory `new_document.trailer` stays clean for subsequent saves.
        let saved_trailer = if let Some(state) = encryption_state.as_ref() {
            let encrypt_id = state
                .encrypt_object_id()
                .expect("encrypt_object_id presence checked by check_incremental_save_supported");
            let mut modified = self.new_document.trailer.clone();
            modified.set(b"Encrypt", Object::Reference(encrypt_id));
            Some(std::mem::replace(&mut self.new_document.trailer, modified))
        } else {
            None
        };

        let xref_start = target.bytes_written;

        // Pick right cross reference stream.
        let write_result: Result<()> = (|| {
            match xref.cross_reference_type {
                XrefType::CrossReferenceTable => {
                    Writer::write_xref(&mut target, &xref)?;
                    self.new_document
                        .write_trailer(&mut target, self.new_document.max_id + 1)?;
                }
                XrefType::CrossReferenceStream => {
                    // Cross Reference Stream instead of XRef and Trailer
                    self.new_document
                        .write_cross_reference_stream(&mut target, &mut xref, false)?;
                }
            }
            // Write `startxref` part of trailer
            write!(target, "\nstartxref\n{xref_start}\n%%EOF")?;
            Ok(())
        })();

        // Restore the original in-memory trailer even if writing failed.
        if let Some(saved) = saved_trailer {
            self.new_document.trailer = saved;
        }

        write_result
    }
}

pub struct Writer;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum XRefStreamFilter {
    ASCIIHexDecode,
    _FlateDecode, //this is generally a Zlib compressed Stream.
    None,
}

impl Writer {
    fn need_separator(object: &Object) -> bool {
        matches!(*object, Null | Boolean(_) | Integer(_) | Real(_) | Reference(_))
    }

    fn need_end_separator(object: &Object) -> bool {
        matches!(
            *object,
            Null | Boolean(_) | Integer(_) | Real(_) | Name(_) | Reference(_) | Object::Stream(_)
        )
    }

    /// Write Cross Reference Table.
    ///
    /// Note: This is different from a "Cross Reference Stream".
    ///
    /// A classic table has no way to locate an object inside an object stream, so an
    /// xref holding live compressed (type-2) entries is rejected with
    /// [`std::io::ErrorKind::Unsupported`] before a single byte is written; turning
    /// such an entry into a free one would present a live object as deleted.
    fn write_xref(file: &mut dyn Write, xref: &Xref) -> Result<()> {
        if xref.entries.values().any(|entry| matches!(entry, XrefEntry::Null)) {
            return Err(crate::xref::null_serialization_error());
        }
        if xref
            .entries
            .values()
            .any(|entry| matches!(entry, XrefEntry::Compressed { .. }))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "a classic cross-reference table cannot locate objects inside object streams; \
                 compressed entries require cross-reference stream authority",
            ));
        }

        writeln!(file, "xref")?;

        let mut xref_section = XrefSection::new(0);
        // Full rewrites supply object 0; sparse incremental callers retain the fallback.
        if let Some(entry @ XrefEntry::Free { .. }) = xref.get(0) {
            xref_section.add_entry(entry.clone());
        } else {
            xref_section.add_unusable_free_entry();
        }

        // Iterate over the actual highest entry instead of `xref.size`:
        // `size` is fixed before object streams (and the xref stream itself)
        // are appended, so entries past it would never reach the table.
        for obj_id in 1..=xref.max_id() {
            if let Some(entry) = xref.get(obj_id) {
                // A section starts at the first *present* id; starting it at
                // a missing id would shift every subsequent entry by one.
                if xref_section.is_empty() {
                    xref_section = XrefSection::new(obj_id);
                }
                match *entry {
                    XrefEntry::Null => return Err(crate::xref::null_serialization_error()),
                    XrefEntry::Normal { offset, generation } => {
                        // Add entry
                        xref_section.add_entry(XrefEntry::Normal { offset, generation });
                    }
                    XrefEntry::Compressed { .. } => {
                        // Rejected above, before any byte was written. Kept as a guard so
                        // a compressed entry can never decay into a free entry here.
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "a classic cross-reference table cannot locate objects inside object streams; \
                             compressed entries require cross-reference stream authority",
                        ));
                    }
                    XrefEntry::Free { next_free, generation } => {
                        xref_section.add_entry(XrefEntry::Free { next_free, generation });
                    }
                    XrefEntry::UnusableFree => {
                        xref_section.add_unusable_free_entry();
                    }
                }
            } else {
                // Skip over `obj_id`, but finish section if not empty.
                if !xref_section.is_empty() {
                    xref_section.write_xref_section(file)?;
                    xref_section = XrefSection::new(0);
                }
            }
        }
        // Print last section
        if !xref_section.is_empty() {
            xref_section.write_xref_section(file)?;
        }
        Ok(())
    }

    /// Create stream for Cross reference stream.
    fn create_xref_steam(xref: &Xref, filter: XRefStreamFilter) -> Result<(Vec<u8>, usize, Object)> {
        if xref.entries.values().any(|entry| matches!(entry, XrefEntry::Null)) {
            return Err(crate::xref::null_serialization_error());
        }
        let mut xref_sections = Vec::new();
        let mut xref_section = XrefSection::new(0);
        if let Some(entry @ XrefEntry::Free { .. }) = xref.get(0) {
            xref_section.add_entry(entry.clone());
        }

        // Iterate over the actual highest entry instead of `xref.size`:
        // `size` is fixed before object streams (and the xref stream itself)
        // are appended, so entries past it would never reach the stream.
        for obj_id in 1..=xref.max_id() {
            if let Some(entry) = xref.get(obj_id) {
                // A section starts at the first *present* id; starting it at
                // a missing id would shift every subsequent entry by one.
                if xref_section.is_empty() {
                    xref_section = XrefSection::new(obj_id);
                }
                xref_section.add_entry(entry.clone());
            } else {
                // Skip over but finish section if not empty
                if !xref_section.is_empty() {
                    xref_sections.push(xref_section);
                    xref_section = XrefSection::new(0);
                }
            }
        }
        // Print last section
        if !xref_section.is_empty() {
            xref_sections.push(xref_section);
        }

        let mut xref_stream = Vec::new();
        let mut xref_index = Vec::new();

        for section in xref_sections {
            // Add indexes to list
            xref_index.push(Integer(section.starting_id as i64));
            xref_index.push(Integer(section.entries.len() as i64));
            // Add entries to stream
            for (obj_id, entry) in (section.starting_id..).zip(section.entries) {
                match entry {
                    XrefEntry::Null => return Err(crate::xref::null_serialization_error()),
                    XrefEntry::Free { next_free, generation } => {
                        // Type 0
                        xref_stream.push(0);
                        xref_stream.extend(next_free.to_be_bytes());
                        xref_stream.extend(generation.to_be_bytes());
                    }
                    XrefEntry::UnusableFree => {
                        // Type 0
                        xref_stream.push(0);
                        xref_stream.extend(obj_id.to_be_bytes());
                        xref_stream.extend(65535_u16.to_be_bytes());
                    }
                    XrefEntry::Normal { offset, generation } => {
                        // Type 1
                        xref_stream.push(1);
                        xref_stream.extend(offset.to_be_bytes());
                        xref_stream.extend(generation.to_be_bytes());
                    }
                    XrefEntry::Compressed { container, index } => {
                        // Type 2
                        xref_stream.push(2);
                        xref_stream.extend(container.to_be_bytes());
                        xref_stream.extend(index.to_be_bytes());
                    }
                }
            }
        }

        // The end of line character should not be counted, added later.
        let stream_length = xref_stream.len();

        if filter == XRefStreamFilter::ASCIIHexDecode {
            xref_stream = xref_stream
                .iter()
                .flat_map(|c| format!("{c:02X}").as_bytes().to_vec())
                .collect::<Vec<u8>>();
        }

        Ok((xref_stream, stream_length, Array(xref_index)))
    }

    fn write_indirect_object<W: Write>(
        file: &mut CountingWrite<&mut W>, id: u32, generation: u16, object: &Object, xref: &mut Xref,
    ) -> Result<()> {
        let offset = file.checked_position_u32()?;
        xref.insert(id, XrefEntry::Normal { offset, generation });
        write!(
            file,
            "{} {} obj\n{}",
            id,
            generation,
            if Writer::need_separator(object) { " " } else { "" }
        )?;
        Writer::write_object(file, object)?;
        writeln!(
            file,
            "{}\nendobj",
            if Writer::need_end_separator(object) { " " } else { "" }
        )?;
        Ok(())
    }

    pub fn write_object(file: &mut dyn Write, object: &Object) -> Result<()> {
        match object {
            Null => file.write_all(b"null"),
            Boolean(value) => {
                if *value {
                    file.write_all(b"true")
                } else {
                    file.write_all(b"false")
                }
            }
            Integer(value) => {
                let mut buf = itoa::Buffer::new();
                file.write_all(buf.format(*value).as_bytes())
            }
            Real(value) => Writer::write_real(file, *value),
            Name(name) => Writer::write_name(file, name),
            String(text, format) => Writer::write_string(file, text, format),
            Array(array) => Writer::write_array(file, array),
            Object::Dictionary(dict) => Writer::write_dictionary(file, dict),
            Object::Stream(stream) => Writer::write_stream(file, stream),
            Reference(id) => write!(file, "{} {} R", id.0, id.1),
        }
    }

    /// Write a `Real` as a PDF real-number token.
    ///
    /// The PDF number grammar has no exponent notation and a token without a
    /// decimal point is an integer, so neither the shortest exponent form nor
    /// the bare integral form of an `f32` is a valid real. The shortest
    /// round-trip decimal digits are taken in scientific form and expanded to
    /// plain decimal notation that always contains a decimal point. Every
    /// finite `f32` therefore keeps both its object variant and its exact value
    /// across a save/load cycle. Non-finite values have no PDF number
    /// representation and are rejected instead of being emitted as `NaN` or
    /// `inf`.
    fn write_real(file: &mut dyn Write, value: f32) -> Result<()> {
        let token = Writer::format_real(value).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cannot write a non-finite Real as a PDF number",
            )
        })?;
        file.write_all(token.as_bytes())
    }

    /// The plain decimal PDF token for a finite `f32`, or `None` when the value
    /// is non-finite.
    fn format_real(value: f32) -> Option<std::string::String> {
        if !value.is_finite() {
            return None;
        }
        expand_scientific_real(&format!("{value:e}"))
    }

    fn write_name(file: &mut dyn Write, name: &[u8]) -> Result<()> {
        file.write_all(b"/")?;
        for &byte in name {
            // white-space and delimiter chars are encoded to # sequences
            // also encode bytes outside of the range 33 (!) to 126 (~)
            if b" \t\n\r\x0C()<>[]{}/%#".contains(&byte) || !(33..=126).contains(&byte) {
                write!(file, "#{byte:02X}")?;
            } else {
                file.write_all(&[byte])?;
            }
        }
        Ok(())
    }

    fn write_string(file: &mut dyn Write, text: &[u8], format: &StringFormat) -> Result<()> {
        match *format {
            // Within a Literal string, backslash (\) and unbalanced parentheses should be escaped.
            // This rule apply to each individual byte in a string object,
            // whether the string is interpreted as single-byte or multiple-byte character codes.
            // If an end-of-line marker appears within a literal string without a preceding backslash, the result is
            // equivalent to \n. So \r also need be escaped.
            StringFormat::Literal => {
                let mut escape_indice = Vec::new();
                let mut parentheses = Vec::new();
                for (index, &byte) in text.iter().enumerate() {
                    match byte {
                        b'(' => parentheses.push(index),
                        b')' => {
                            if !parentheses.is_empty() {
                                parentheses.pop();
                            } else {
                                escape_indice.push(index);
                            }
                        }
                        b'\\' | b'\r' => escape_indice.push(index),
                        _ => continue,
                    }
                }
                escape_indice.append(&mut parentheses);

                file.write_all(b"(")?;
                if !escape_indice.is_empty() {
                    for (index, &byte) in text.iter().enumerate() {
                        if escape_indice.contains(&index) {
                            file.write_all(b"\\")?;
                            file.write_all(&[if byte == b'\r' { b'r' } else { byte }])?;
                        } else {
                            file.write_all(&[byte])?;
                        }
                    }
                } else {
                    file.write_all(text)?;
                }
                file.write_all(b")")?;
            }
            StringFormat::Hexadecimal => {
                file.write_all(b"<")?;
                for &byte in text {
                    write!(file, "{byte:02X}")?;
                }
                file.write_all(b">")?;
            }
        }
        Ok(())
    }

    fn write_array(file: &mut dyn Write, array: &[Object]) -> Result<()> {
        file.write_all(b"[")?;
        let mut first = true;
        for object in array {
            if first {
                first = false;
            } else if Writer::need_separator(object) {
                file.write_all(b" ")?;
            }
            Writer::write_object(file, object)?;
        }
        file.write_all(b"]")?;
        Ok(())
    }

    fn write_dictionary(file: &mut dyn Write, dictionary: &Dictionary) -> Result<()> {
        file.write_all(b"<<")?;
        for (key, value) in dictionary {
            Writer::write_name(file, key)?;
            if Writer::need_separator(value) {
                file.write_all(b" ")?;
            }
            Writer::write_object(file, value)?;
        }
        file.write_all(b">>")?;
        Ok(())
    }

    fn write_stream(file: &mut dyn Write, stream: &Stream) -> Result<()> {
        Writer::write_dictionary(file, &stream.dict)?;
        file.write_all(b"stream\n")?;
        file.write_all(&stream.content)?;
        file.write_all(b"\nendstream")?;
        Ok(())
    }

    /// Write Binary mark as follows: %{binary_mark[4]}\n -> %Çì¢ or Hex(%25 c3 87 c3 ac)
    ///
    /// Note: Specified in  ISO 19005-2:2011, ISO 19005-3:2012
    /// headerByte1 > 127 && headerByte2 > 127 && headerByte3 > 127 && headerByte4 > 127
    fn write_binary_mark(file: &mut dyn Write, binary_mark: &[u8]) -> Result<()> {
        if binary_mark.iter().all(|&byte| byte >= 128) {
            file.write_all(b"%")?;
            file.write_all(binary_mark)?;
            file.write_all(b"\n")?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid binary mark",
            ));
        }

        Ok(())
    }
}

/// Expand a shortest-round-trip scientific `f32` representation, as produced
/// by `format!("{value:e}")`, into plain decimal notation.
///
/// The input has the form `[-]D[.D]e[-]N`; the output is the same decimal
/// value written without an exponent and always contains a decimal point, so
/// it is a valid PDF real token. `None` is returned for input that does not
/// have the expected shape; the caller turns that into a write error.
fn expand_scientific_real(scientific: &str) -> Option<std::string::String> {
    let (mantissa, exponent) = scientific.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let (digits, fractional) = match mantissa.split_once('.') {
        Some((integer, fraction)) => (format!("{integer}{fraction}"), fraction.len()),
        None => (mantissa.to_string(), 0),
    };
    // Position of the decimal point relative to the start of `digits`.
    let point = i32::try_from(digits.len()).ok()? - i32::try_from(fractional).ok()? + exponent;

    let mut token = std::string::String::with_capacity(sign.len() + digits.len() + 4);
    token.push_str(sign);
    if point <= 0 {
        token.push_str("0.");
        token.extend(std::iter::repeat_n('0', usize::try_from(-point).ok()?));
        token.push_str(&digits);
    } else {
        let point = usize::try_from(point).ok()?;
        if point < digits.len() {
            token.push_str(&digits[..point]);
            token.push('.');
            token.push_str(&digits[point..]);
        } else {
            token.push_str(&digits);
            token.extend(std::iter::repeat_n('0', point - digits.len()));
            token.push_str(".0");
        }
    }
    Some(token)
}

pub struct CountingWrite<W: Write> {
    inner: W,
    bytes_written: usize,
}

impl<W: Write> CountingWrite<W> {
    fn checked_position_u32(&self) -> Result<u32> {
        u32::try_from(self.bytes_written).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "PDF object offset exceeds the supported u32 range",
            )
        })
    }
}

impl<W: Write> Write for CountingWrite<W> {
    #[inline]
    fn write(&mut self, buffer: &[u8]) -> Result<usize> {
        let result = self.inner.write(buffer);
        if let Ok(bytes) = result {
            match self.bytes_written.checked_add(bytes) {
                Some(total) => self.bytes_written = total,
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "PDF output byte count exceeds the supported range",
                    ));
                }
            }
        }
        result
    }

    #[inline]
    fn write_all(&mut self, buffer: &[u8]) -> Result<()> {
        let total = self.bytes_written.checked_add(buffer.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "PDF output byte count exceeds the supported range",
            )
        })?;
        self.inner.write_all(buffer)?;
        self.bytes_written = total;
        Ok(())
    }

    #[inline]
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

#[test]
fn save_document() {
    let mut doc = Document::with_version("1.5");
    doc.objects.insert((1, 0), Null);
    doc.objects.insert((2, 0), Boolean(true));
    doc.objects.insert((3, 0), Integer(3));
    doc.objects.insert((4, 0), Real(0.5));
    doc.objects
        .insert((5, 0), String("text((\r)".as_bytes().to_vec(), StringFormat::Literal));
    doc.objects.insert(
        (6, 0),
        String("text((\r)".as_bytes().to_vec(), StringFormat::Hexadecimal),
    );
    doc.objects.insert((7, 0), Name(b"name \t".to_vec()));
    doc.objects.insert((8, 0), Reference((1, 0)));
    doc.objects
        .insert((9, 2), Array(vec![Integer(1), Integer(2), Integer(3)]));
    doc.objects
        .insert((11, 0), Stream(Stream::new(Dictionary::new(), vec![0x41, 0x42, 0x43])));
    let mut dict = Dictionary::new();
    dict.set("A", Null);
    dict.set("B", false);
    dict.set("C", Name(b"name".to_vec()));
    doc.objects.insert((12, 0), Object::Dictionary(dict));
    doc.max_id = 12;

    // Create temporary folder to store file.
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("test_0_save.pdf");
    doc.save(&file_path).unwrap();
    // Check if file was created.
    assert!(file_path.exists());
    // Check if path is file
    assert!(file_path.is_file());
    // Check if the file is above 400 bytes (should be about 610 bytes)
    assert!(file_path.metadata().unwrap().len() > 400);
}

#[test]
fn raw_null_authority_is_rejected_by_both_writers() {
    for id in [0, 2] {
        let mut xref = Xref::new(3, XrefType::CrossReferenceStream);
        xref.insert(
            1,
            XrefEntry::Normal {
                offset: 9,
                generation: 0,
            },
        );
        xref.insert(id, XrefEntry::Null);
        let mut bytes = Vec::new();
        assert_eq!(
            Writer::write_xref(&mut bytes, &xref).unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert!(bytes.is_empty());
        assert_eq!(
            Writer::create_xref_steam(&xref, XRefStreamFilter::None)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
    }
}

#[test]
fn write_xref_rejects_compressed_entries_before_emitting_bytes() {
    let mut xref = Xref::new(4, XrefType::CrossReferenceTable);
    xref.insert(
        1,
        XrefEntry::Normal {
            offset: 9,
            generation: 0,
        },
    );
    xref.insert(2, XrefEntry::Compressed { container: 3, index: 1 });
    xref.insert(
        3,
        XrefEntry::Normal {
            offset: 42,
            generation: 0,
        },
    );

    let mut buffer = Vec::new();
    let error = Writer::write_xref(&mut buffer, &xref).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(
        buffer.is_empty(),
        "a rejected table must not emit a single byte, not even the 'xref' keyword"
    );
}

#[cfg(test)]
fn assert_normal_offset(entry: Option<&XrefEntry>, expected: u32) {
    match entry {
        Some(XrefEntry::Normal { offset, generation: 0 }) => assert_eq!(*offset, expected),
        other => panic!("unexpected xref entry: {other:?}"),
    }
}

#[test]
fn checked_position_u32_accepts_u32_max() {
    let mut sink = Vec::new();
    let file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize,
    };
    assert_eq!(file.checked_position_u32().unwrap(), u32::MAX);
}

#[cfg(target_pointer_width = "64")]
#[test]
fn checked_position_u32_rejects_above_u32_max() {
    let mut sink = Vec::new();
    let file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize + 1,
    };
    let error = file.checked_position_u32().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn write_indirect_object_accepts_u32_max_offset() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize,
    };
    let mut xref = Xref::new(2, XrefType::CrossReferenceTable);
    Writer::write_indirect_object(&mut file, 1, 0, &Object::Integer(7), &mut xref).unwrap();
    assert_normal_offset(xref.get(1), u32::MAX);
    let text = std::str::from_utf8(&sink).unwrap();
    assert!(text.contains("1 0 obj"), "object bytes must reach the sink");
}

#[cfg(target_pointer_width = "64")]
#[test]
fn write_indirect_object_rejects_unrepresentable_offset() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize + 1,
    };
    let mut xref = Xref::new(2, XrefType::CrossReferenceTable);
    let error = Writer::write_indirect_object(&mut file, 1, 0, &Object::Integer(7), &mut xref).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(xref.get(1).is_none(), "no xref authority may be inserted");
    assert!(sink.is_empty(), "no object bytes may be emitted");
}

#[test]
fn classic_table_serializes_u32_max_offset() {
    let mut xref = Xref::new(2, XrefType::CrossReferenceTable);
    xref.insert(
        1,
        XrefEntry::Normal {
            offset: u32::MAX,
            generation: 0,
        },
    );
    let mut buffer = Vec::new();
    Writer::write_xref(&mut buffer, &xref).unwrap();
    let text = std::string::String::from_utf8(buffer).unwrap();
    assert!(text.starts_with("xref\n"));
    assert!(text.contains("0000000000 65535 f \n"));
    assert!(text.contains("4294967295 00000 n \n"));
}

#[cfg(target_pointer_width = "64")]
#[test]
fn classic_footer_startxref_is_not_u32_constrained() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize + 1,
    };
    let mut xref = Xref::new(2, XrefType::CrossReferenceTable);
    xref.insert(
        1,
        XrefEntry::Normal {
            offset: 9,
            generation: 0,
        },
    );
    Writer::write_xref(&mut file, &xref).unwrap();
    let xref_start = file.bytes_written;
    assert!(xref_start as u64 > u64::from(u32::MAX));
    write!(file, "\nstartxref\n{xref_start}\n%%EOF").unwrap();
    let text = std::string::String::from_utf8(sink).unwrap();
    assert!(text.contains(&format!("startxref\n{xref_start}\n%%EOF")));
}

#[test]
fn xref_stream_self_offset_at_u32_max() {
    let mut doc = Document::with_version("1.5");
    doc.max_id = 1;
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize,
    };
    let mut xref = Xref::new(2, XrefType::CrossReferenceStream);
    doc.write_cross_reference_stream(&mut file, &mut xref, true).unwrap();
    assert_normal_offset(xref.get(doc.max_id), u32::MAX);
    let type1_u32_max: [u8; 7] = [1, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00];
    assert!(
        sink.windows(type1_u32_max.len()).any(|window| window == type1_u32_max),
        "raw type-1 field 2 must encode offset FF FF FF FF"
    );
}

#[cfg(target_pointer_width = "64")]
#[test]
fn xref_stream_self_offset_above_u32_max_rejected_before_mutation() {
    let mut doc = Document::with_version("1.5");
    doc.max_id = 1;
    doc.trailer.set("Root", Reference((1, 0)));
    let trailer_before = doc.trailer.clone();
    let max_id_before = doc.max_id;
    let mut xref = Xref::new(2, XrefType::CrossReferenceStream);
    xref.insert(
        1,
        XrefEntry::Normal {
            offset: 9,
            generation: 0,
        },
    );
    let xref_entries_before = xref.entries.clone();

    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize + 1,
    };
    let error = doc
        .write_cross_reference_stream(&mut file, &mut xref, true)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(sink.is_empty(), "the helper must not emit any byte");
    assert_eq!(doc.max_id, max_id_before, "max_id must stay untouched");
    assert_eq!(doc.trailer, trailer_before, "trailer must stay untouched");
    assert_eq!(
        format!("{xref_entries_before:?}"),
        format!("{:?}", xref.entries),
        "xref entries must stay untouched"
    );
}

#[test]
fn full_save_footer_matches_xref_stream_position() {
    let mut doc = Document::with_version("1.5");
    doc.objects.insert((1, 0), Object::Integer(1));
    doc.objects.insert((2, 0), Object::Integer(2));
    doc.max_id = 2;
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    let text = std::string::String::from_utf8_lossy(&out);
    let footer_pos = text.find("startxref\n").unwrap() + "startxref\n".len();
    let footer_end = text[footer_pos..].find('\n').unwrap() + footer_pos;
    let startxref: usize = text[footer_pos..footer_end].parse().unwrap();
    let expected = format!("{} 0 obj", doc.max_id);
    assert!(
        out[startxref..].starts_with(expected.as_bytes()),
        "the checked helper position and the printed startxref must both land on the xref stream object"
    );
}

#[test]
fn incremental_mode_xref_stream_uses_helper_position_and_full_width_footer() {
    let mut doc = Document::with_version("1.5");
    doc.max_id = 1;
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize,
    };
    let mut xref = Xref::new(2, XrefType::CrossReferenceStream);
    let xref_start = file.bytes_written;
    doc.write_cross_reference_stream(&mut file, &mut xref, false).unwrap();
    assert_normal_offset(xref.get(doc.max_id), u32::MAX);
    assert_eq!(
        doc.trailer.get(b"Size").unwrap(),
        &Object::Integer(i64::from(doc.max_id + 1))
    );
    write!(file, "\nstartxref\n{xref_start}\n%%EOF").unwrap();
    let text = std::string::String::from_utf8_lossy(&sink);
    assert!(text.contains("startxref\n4294967295\n%%EOF"));
}

#[test]
fn generated_stream_object_accepts_u32_max_offset() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize,
    };
    let mut xref = Xref::new(6, XrefType::CrossReferenceStream);
    let stream = Object::Stream(Stream::new(Dictionary::new(), vec![0x41, 0x42, 0x43]));
    Writer::write_indirect_object(&mut file, 5, 0, &stream, &mut xref).unwrap();
    assert_normal_offset(xref.get(5), u32::MAX);
    let text = std::str::from_utf8(&sink).unwrap();
    assert!(text.contains("5 0 obj"));
    assert!(sink.windows(3).any(|window| window == [0x41, 0x42, 0x43]));
}

#[cfg(target_pointer_width = "64")]
#[test]
fn generated_stream_object_rejects_unrepresentable_offset() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: u32::MAX as usize + 1,
    };
    let mut xref = Xref::new(6, XrefType::CrossReferenceStream);
    let stream = Object::Stream(Stream::new(Dictionary::new(), vec![0x41, 0x42, 0x43]));
    let error = Writer::write_indirect_object(&mut file, 5, 0, &stream, &mut xref).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(xref.get(5).is_none());
    assert!(sink.is_empty());
}

#[test]
fn counting_write_write_overflow_returns_error_without_wrap() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: usize::MAX,
    };
    let error = file.write(b"payload").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(file.bytes_written, usize::MAX, "the counter must not wrap");
    assert_eq!(sink, b"payload", "bytes already reached the sink, no rollback");
}

#[test]
fn counting_write_zero_len_write_at_counter_max_is_ok() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: usize::MAX,
    };
    assert_eq!(file.write(b"").unwrap(), 0);
    assert_eq!(file.bytes_written, usize::MAX);
    assert!(sink.is_empty());
}

#[test]
fn counting_write_counts_actually_written_bytes() {
    struct OneByteAtATime(Vec<u8>);
    impl Write for OneByteAtATime {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            let accepted = buf.len().min(1);
            self.0.extend_from_slice(&buf[..accepted]);
            Ok(accepted)
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }
    let mut sink = OneByteAtATime(Vec::new());
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: 0,
    };
    assert_eq!(file.write(b"abc").unwrap(), 1);
    assert_eq!(file.bytes_written, 1);
    assert_eq!(sink.0, b"a");
}

#[test]
fn counting_write_underlying_error_propagates() {
    struct FailingSink;
    impl Write for FailingSink {
        fn write(&mut self, _: &[u8]) -> Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "sink failed"))
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }
    let mut file = CountingWrite {
        inner: FailingSink,
        bytes_written: 10,
    };
    let error = file.write(b"abc").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(file.bytes_written, 10);
}

#[test]
fn counting_write_all_overflow_is_rejected_before_the_underlying_write() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: usize::MAX,
    };
    let error = file.write_all(b"payload").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        sink.is_empty(),
        "an unrepresentable future count must not reach the sink"
    );
}

#[test]
fn counting_write_all_advances_only_after_successful_write() {
    let mut sink = Vec::new();
    let mut file = CountingWrite {
        inner: &mut sink,
        bytes_written: usize::MAX - 3,
    };
    file.write_all(b"abc").unwrap();
    assert_eq!(file.bytes_written, usize::MAX);
    assert_eq!(sink, b"abc");
}

#[test]
fn counting_write_all_underlying_error_does_not_advance_counter() {
    struct FailingSink;
    impl Write for FailingSink {
        fn write(&mut self, _: &[u8]) -> Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "sink failed"))
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }
    let mut file = CountingWrite {
        inner: FailingSink,
        bytes_written: 10,
    };
    let error = file.write_all(b"abc").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(file.bytes_written, 10);
}

/// A deterministic grid across sign, exponent and mantissa patterns, plus
/// exact boundary bit patterns. Not an exhaustive 2^32 sweep.
#[cfg(test)]
fn sampled_finite_real_bits() -> Vec<u32> {
    let mut bits = Vec::new();
    for sign in [0u32, 0x8000_0000] {
        for exponent in [0u32, 1, 2, 3, 4, 7, 15, 30, 60, 100, 126, 127, 128, 150, 200, 254] {
            for mantissa in [0u32, 1, 2, 0x40_0000, 0x7f_ffff, 0x12_3456] {
                bits.push(sign | (exponent << 23) | mantissa);
            }
        }
    }
    bits.sort_unstable();
    bits.dedup();
    bits
}

#[cfg(test)]
fn assert_real_token(value: f32) {
    let mut buffer = Vec::new();
    Writer::write_object(&mut buffer, &Real(value)).unwrap();
    let token = std::str::from_utf8(&buffer).unwrap();
    assert!(
        token.contains('.'),
        "{value:?} serialized as {token:?} without a decimal point"
    );
    assert!(
        !token.contains('e') && !token.contains('E'),
        "{value:?} serialized as {token:?} with an exponent"
    );
    let parsed: f32 = token
        .parse()
        .unwrap_or_else(|error| panic!("{token:?} is not an f32: {error}"));
    assert_eq!(
        parsed.to_bits(),
        value.to_bits(),
        "{value:?} serialized as {token:?} and parsed back as {parsed:?}"
    );
}

#[test]
fn required_real_values_keep_real_syntax_and_round_trip() {
    for value in [0.0, -0.0, 1.0, -1.0, 2.0, 1.5, -1.5, 0.5, 0.1] {
        assert_real_token(value);
    }
}

#[test]
fn integral_reals_are_not_written_as_integers() {
    let mut buffer = Vec::new();
    Writer::write_object(&mut buffer, &Real(1.0)).unwrap();
    assert_eq!(buffer, b"1.0");
    buffer.clear();
    Writer::write_object(&mut buffer, &Real(-1.0)).unwrap();
    assert_eq!(buffer, b"-1.0");
    buffer.clear();
    Writer::write_object(&mut buffer, &Real(0.0)).unwrap();
    assert_eq!(buffer, b"0.0");
}

#[test]
fn negative_zero_keeps_its_sign_and_bits() {
    let mut buffer = Vec::new();
    Writer::write_object(&mut buffer, &Real(-0.0)).unwrap();
    assert_eq!(buffer, b"-0.0");
    let parsed: f32 = std::str::from_utf8(&buffer).unwrap().parse().unwrap();
    assert_eq!(parsed.to_bits(), (-0.0f32).to_bits());
    assert_ne!(parsed.to_bits(), 0.0f32.to_bits());
}

#[test]
fn exponent_prone_and_boundary_reals_round_trip() {
    for value in [
        f32::MIN_POSITIVE,
        f32::EPSILON,
        f32::MAX,
        -f32::MAX,
        f32::MIN,
        1.0e-20,
        1.0e-30,
        1.0e20,
        1.0e30,
        1.0e38,
        f32::from_bits(0x0000_0001),
        f32::from_bits(0x8000_0001),
        f32::from_bits(0x003f_ffff),
        f32::from_bits(0x807f_ffff),
    ] {
        assert_real_token(value);
    }
}

#[test]
fn sampled_finite_real_bits_keep_real_syntax_and_round_trip() {
    let bits = sampled_finite_real_bits();
    assert!(bits.len() > 100, "the sample must span the sign/exponent/mantissa grid");
    for bit_pattern in bits {
        assert_real_token(f32::from_bits(bit_pattern));
    }
}

#[test]
fn nonfinite_reals_are_rejected_without_writing_a_token() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut buffer = Vec::new();
        let error = Writer::write_object(&mut buffer, &Real(value)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(buffer.is_empty(), "{value:?} wrote {buffer:?} before failing");
    }
}

#[test]
fn nonfinite_reals_are_rejected_inside_containers() {
    for object in [
        Array(vec![Real(f32::NAN)]),
        Object::Dictionary({
            let mut dict = Dictionary::new();
            dict.set("Value", Real(f32::INFINITY));
            dict
        }),
    ] {
        let mut buffer = Vec::new();
        let error = Writer::write_object(&mut buffer, &object).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}

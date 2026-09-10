use crate::ObjectStreamConfig;

/// Options for saving PDF documents
#[derive(Debug, Clone, Default)]
pub struct SaveOptions {
    /// Enable object streams for compressing non-stream objects.
    ///
    /// Object streams can only be emitted when the selected output cross-reference
    /// representation can encode type-2 compressed entries, which lopdf supports
    /// through cross-reference-stream output. The PDF specification also permits
    /// hybrid-reference files that pair a classic cross-reference table with a
    /// `/XRefStm`, but lopdf does not write hybrid-reference files: a full save that
    /// would emit object streams while keeping a classic cross-reference table fails
    /// with [`std::io::ErrorKind::Unsupported`] before anything is written or modified.
    ///
    /// Configuration is validated only when it would actually be used. If an object
    /// stream would be built and `object_stream_config.max_objects_per_stream` is zero,
    /// the save fails with [`std::io::ErrorKind::InvalidInput`]. The builder normalizes
    /// a zero to the default capacity, so only direct construction of the configuration
    /// struct can produce this. An encrypted document skips newly constructed object
    /// streams; its objects are serialized individually and the selected cross-reference
    /// type still applies, regardless of the capacity.
    pub use_object_streams: bool,

    /// Enable cross-reference streams instead of the document's current
    /// cross-reference representation.
    ///
    /// `true` forces cross-reference-stream output. `false` preserves the
    /// representation the document already uses; it does NOT force classic
    /// cross-reference-table output, so a document loaded from a
    /// cross-reference-stream file stays on cross-reference streams.
    pub use_xref_streams: bool,

    /// Enable linearization (fast web view)
    pub linearize: bool,

    /// Configuration for object streams
    pub object_stream_config: ObjectStreamConfig,
}

impl SaveOptions {
    /// Create a builder for SaveOptions
    pub fn builder() -> SaveOptionsBuilder {
        SaveOptionsBuilder::default()
    }
}

/// Builder for SaveOptions
#[derive(Default)]
pub struct SaveOptionsBuilder {
    use_object_streams: bool,
    use_xref_streams: bool,
    linearize: bool,
    max_objects_per_stream: usize,
    compression_level: u32,
}

impl SaveOptionsBuilder {
    /// Enable or disable object streams
    ///
    /// See [`SaveOptions::use_object_streams`] for the cross-reference representation
    /// object streams require and the zero-capacity restriction.
    pub fn use_object_streams(mut self, value: bool) -> Self {
        self.use_object_streams = value;
        self
    }

    /// Enable or disable cross-reference streams
    ///
    /// `true` forces cross-reference-stream output; `false` keeps the document's
    /// current cross-reference representation.
    pub fn use_xref_streams(mut self, value: bool) -> Self {
        self.use_xref_streams = value;
        self
    }

    /// Enable or disable linearization
    pub fn linearize(mut self, value: bool) -> Self {
        self.linearize = value;
        self
    }

    /// Set maximum objects per stream
    ///
    /// This is a requested maximum: generated streams may be split below it.
    /// lopdf currently limits generated streams to 65,536 members so their type-2
    /// member indices fit its `u16` representation; this is not a PDF format limit.
    ///
    /// A value of zero is normalized to the default capacity, so a configuration
    /// produced by this builder always holds objects.
    pub fn max_objects_per_stream(mut self, value: usize) -> Self {
        self.max_objects_per_stream = value;
        self
    }

    /// Set compression level (0-9)
    pub fn compression_level(mut self, value: u32) -> Self {
        self.compression_level = value;
        self
    }

    /// Build the SaveOptions
    pub fn build(self) -> SaveOptions {
        SaveOptions {
            use_object_streams: self.use_object_streams,
            use_xref_streams: self.use_xref_streams,
            linearize: self.linearize,
            object_stream_config: ObjectStreamConfig {
                max_objects_per_stream: if self.max_objects_per_stream == 0 {
                    100
                } else {
                    self.max_objects_per_stream
                },
                compression_level: self.compression_level,
            },
        }
    }
}

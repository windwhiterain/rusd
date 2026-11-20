//! Rusd document format (`.rusd`): a USD **layer** written in TOML.
//!
//! This is not a second scene model. A `.rusd` file is the same layer the `usda` format
//! carries — prims, types, attributes, references, layer metadata — spelled as TOML
//! tables instead of a brace grammar, and it takes its place in a composition exactly
//! where any other layer does: as the root of a stage, as a sublayer, as the target of a
//! `references` arc, as a cook product, as an asset. Registering it here is what makes
//! that true: an engine that opens layers opens this one.
//!
//! The format is only the wire. What a document *means* — which table is a prim, which
//! key is a type, what type a value has once the schema table has spoken — belongs to the
//! host that authored it, so [`set_decoder`] and [`set_encoder`] are how that meaning
//! arrives: no decoder, no `.rusd` layer. Keeping the split this way is deliberate. A
//! grammar with no model behind it (an "openusd reads TOML" that guessed at keys) would
//! be a second, silently diverging definition of the format.

use std::fmt;
use std::sync::OnceLock;

use crate::{ar, sdf, tf};

/// The file extension this format claims.
pub const EXTENSION: &str = "rusd";

/// Decode a document: its text, the location it came from, and the layer's data.
///
/// `location` is the path the bytes were read from, which is what a relative asset path
/// inside the document anchors against.
pub type Decoder = fn(text: &str, location: &str) -> Result<sdf::LayerData, String>;

/// Encode a layer's data as a document: the counterpart of [`Decoder`], and the reason
/// the split is the host's — writing needs the schema table to know a value's type.
pub type Encoder = fn(data: &dyn sdf::AbstractData) -> Result<String, String>;

static DECODER: OnceLock<Decoder> = OnceLock::new();
static ENCODER: OnceLock<Encoder> = OnceLock::new();

/// Install the decoder every `.rusd` read goes through. The first call wins: the decoder
/// is the host's, and two of them would be two meanings for one extension.
pub fn set_decoder(decoder: Decoder) {
    let _ = DECODER.set(decoder);
}

/// The installed decoder, if a host has installed one.
pub fn decoder() -> Option<Decoder> {
    DECODER.get().copied()
}

/// Install the encoder a `.rusd` write goes through, for the same reason.
pub fn set_encoder(encoder: Encoder) {
    let _ = ENCODER.set(encoder);
}

/// TOML scene description (`.rusd`) as an [`sdf::FileFormat`].
pub struct RusdFileFormat;

impl sdf::FileFormat for RusdFileFormat {
    fn format_id(&self) -> tf::Token {
        tf::Token::new(EXTENSION)
    }

    fn extensions(&self) -> &[&str] {
        &[EXTENSION]
    }

    /// Reading and writing. **Editing** is not: a document is written from a schema table,
    /// so it is a format a pipeline produces and consumes, not one an edit target
    /// accumulates opinions in.
    fn caps(&self) -> sdf::FileFormatCaps {
        sdf::FileFormatCaps::READ.union(sdf::FileFormatCaps::WRITE)
    }

    /// A document never claims bytes by content: `.rusd` is chosen by extension, and the
    /// `.usd` disambiguation this would serve has no text/binary split here.
    fn matches_content(&self, _prefix: &[u8]) -> bool {
        false
    }

    fn read_bytes(
        &self,
        bytes: ar::AssetBuffer,
        source_name: &str,
    ) -> Result<sdf::LayerData, sdf::FormatError> {
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| sdf::FormatError::Decode(Box::new(error)))?;
        let decoder = decoder().ok_or_else(|| {
            sdf::FormatError::Decode(Box::new(RusdFormatError(
                "no Rusd document decoder is installed in this process: the host that \
                 authors .rusd files has to install one (`openusd::rusd::set_decoder`) \
                 before a stage opens one"
                    .to_owned(),
            )))
        })?;
        decoder(text, source_name).map_err(|message| {
            sdf::FormatError::Decode(Box::new(RusdFormatError(format!(
                "{source_name}: {message}"
            ))))
        })
    }

    fn write(
        &self,
        data: &dyn sdf::AbstractData,
        sink: &mut dyn sdf::WriteSeek,
    ) -> Result<(), sdf::FormatError> {
        let encoder = ENCODER
            .get()
            .copied()
            .ok_or_else(|| sdf::FormatError::Encode {
                reason: "no Rusd document encoder is installed in this process".into(),
            })?;
        let text = encoder(data).map_err(|message| sdf::FormatError::Encode {
            reason: message.into_boxed_str(),
        })?;
        std::io::Write::write_all(sink, text.as_bytes())?;
        Ok(())
    }
}

/// A refusal to read or write a document, carried as the format's own error.
#[derive(Debug)]
struct RusdFormatError(String);

impl fmt::Display for RusdFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RusdFormatError {}

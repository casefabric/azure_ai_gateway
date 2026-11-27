//! OpenAI `file` content parts, as the Vercel AI SDK (the Mendix Agent Editor's client) sends them:
//! `{"type": "file", "file": {"filename": "x.pdf", "file_data": "data:application/pdf;base64,…"}}`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// An inline file from a `file` content part.
pub struct File<'a> {
    pub filename: Option<&'a str>,
    pub media_type: &'a str,
    /// Base64-encoded contents.
    pub data: &'a str,
}

impl<'a> File<'a> {
    /// Reads the `file` object of a content part. `file_data` is a data URL; bare base64 is taken
    /// to be a PDF (the only kind OpenAI accepts). Uploaded `file_id`s can't be resolved here.
    pub fn from_part(file: &'a Value) -> Result<Self, String> {
        let Some(file_data) = file["file_data"].as_str() else {
            return Err(match file["file_id"].as_str() {
                Some(id) => {
                    format!("File IDs are not supported (got '{id}'); send file_data instead")
                }
                None => "File content part without file_data".into(),
            });
        };
        let (media_type, data) = file_data
            .strip_prefix("data:")
            .and_then(|url| url.split_once(";base64,"))
            .unwrap_or(("application/pdf", file_data));
        Ok(File {
            filename: file["filename"].as_str(),
            media_type,
            data,
        })
    }

    pub fn is_pdf(&self) -> bool {
        self.media_type == "application/pdf"
    }

    pub fn is_text(&self) -> bool {
        self.media_type.starts_with("text/")
    }

    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        STANDARD
            .decode(self.data)
            .map_err(|e| format!("{}: invalid base64 file data: {e}", self.name()))
    }

    /// The contents of a text file.
    pub fn text(&self) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.bytes()?).into_owned())
    }

    /// The text of a PDF (for backends that can't read PDFs themselves).
    pub fn pdf_text(&self) -> Result<String, String> {
        let bytes = self.bytes()?;
        // pdf-extract panics on some malformed PDFs.
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes))
            .map_err(|_| format!("{}: could not read the PDF", self.name()))?
            .map_err(|e| format!("{}: could not read the PDF: {e}", self.name()))
    }

    pub fn name(&self) -> &str {
        self.filename.unwrap_or("file")
    }
}

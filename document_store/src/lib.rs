use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use anyhow::{Error, Result};
use lsp_types::Uri;
use tree_sitter::Parser;

use parser::Tree;

mod line_index;

pub use line_index::LineIndex;

/// A parsed document, together with everything derived from the exact text it
/// was parsed from. Documents are shared rather than copied: whoever reuses a
/// cached parse receives another handle to the same document.
#[derive(Debug)]
pub struct Document {
    pub text: Box<str>,
    pub tree: Tree,
    pub line_index: LineIndex,
    /// The version the client last reported for this buffer, or `None` for text
    /// read from disk, for which the client reported no version.
    pub version: Option<i32>,
}

impl Document {
    pub fn new(text: String, version: Option<i32>) -> Result<Document> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_spml::language())?;
        return match parser.parse(&text, None) {
            Some(tree) => Tree::new(tree, &text).map(|tree| Document {
                line_index: LineIndex::new(&text),
                text: text.into(),
                tree,
                version,
            }),
            None => Err(anyhow::anyhow!("failed to parse text: {}", text)),
        };
    }

    fn from_uri(uri: &Uri) -> Result<Document> {
        return match Path::new(uri.path().as_str()) {
            path if path.exists() => fs::read_to_string(path.to_owned())
                .map(|text| Document::new(text, None))
                .map_err(Error::from),
            path => Err(anyhow::anyhow!("file {:?} does not exist", path)),
        }?;
    }
}

/// The documents the server knows about, each held under the version the client
/// last reported for it.
///
/// Parsing dominates the cost of answering a request, and a single keystroke
/// makes the client ask for semantic tokens, diagnostics, code actions and
/// symbols at once. Keying the parse by version collapses that burst into one
/// parse: every request behind it reuses the document the first one produced.
#[derive(Debug, Default)]
struct Store {
    documents: HashMap<Uri, Arc<Document>>,
}

impl Store {
    /// The document for `uri`, reading and parsing the file on disk when the
    /// store holds none - which is the case for a file the client never opened,
    /// such as the target of a go-to-definition.
    fn get_or_load(&mut self, uri: &Uri) -> Result<Arc<Document>> {
        return match self.documents.get(uri) {
            Some(document) => Ok(document.clone()),
            None => Ok(self.insert(uri, Document::from_uri(uri)?)),
        };
    }

    /// Records `text` as the client's buffer for `uri` at `version`, parsing it
    /// unless the store already holds that version.
    fn update(&mut self, uri: &Uri, version: i32, text: String) -> Result<Arc<Document>> {
        return match self.documents.get(uri) {
            Some(document) if document.version == Some(version) => Ok(document.clone()),
            _ => Ok(self.insert(uri, Document::new(text, Some(version))?)),
        };
    }

    /// Records that the client saved `uri`. What the client wrote is the buffer
    /// the store already holds, so the parse survives the save; disk is read
    /// only when the client sent no text and the store holds nothing to retain.
    fn save(&mut self, uri: &Uri, text: Option<String>) -> Result<Arc<Document>> {
        return match text {
            None => self.get_or_load(uri),
            Some(text) => match self.documents.get(uri) {
                Some(document) if *document.text == *text => Ok(document.clone()),
                // Somebody other than the client wrote the file, so no version
                // the client reported describes this text.
                _ => Ok(self.insert(uri, Document::new(text, None)?)),
            },
        };
    }

    /// Releases `uri`. Without this a long session grows by every document it
    /// ever touched.
    fn close(&mut self, uri: &Uri) -> bool {
        return self.documents.remove(uri).is_some();
    }

    fn insert(&mut self, uri: &Uri, document: Document) -> Arc<Document> {
        let document = Arc::new(document);
        self.documents.insert(uri.clone(), document.clone());
        return document;
    }
}

fn store() -> MutexGuard<'static, Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    return STORE
        .get_or_init(|| Mutex::new(Store::default()))
        .lock()
        .expect("document_store mutex poisoned");
}

pub fn get_or_load(uri: &Uri) -> Result<Arc<Document>> {
    return store().get_or_load(uri);
}

pub fn update(uri: &Uri, version: i32, text: String) -> Result<Arc<Document>> {
    return store().update(uri, version, text);
}

pub fn save(uri: &Uri, text: Option<String>) -> Result<Arc<Document>> {
    return store().save(uri, text);
}

pub fn close(uri: &Uri) -> bool {
    return store().close(uri);
}

#[cfg(test)]
mod tests {
    use std::{env, fs, process, sync::Arc};

    use lsp_types::{Position, Uri};

    use super::{Document, Store};

    const TEXT: &str = "<sp:print name=\"ä\"/>\n";
    const CHANGED_TEXT: &str = "<sp:print name=\"ö\"/>\n";

    fn uri(path: &str) -> Uri {
        return format!("file://{}", path)
            .parse::<Uri>()
            .expect("test uri should parse");
    }

    #[test]
    fn test_a_repeated_version_reuses_the_parse() {
        let mut store = Store::default();
        let uri = uri("/unopened/repeated-version.spml");

        let first = store.update(&uri, 1, TEXT.to_string()).expect("parses");
        let second = store.update(&uri, 1, TEXT.to_string()).expect("parses");

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn test_requests_against_an_unchanged_buffer_reuse_the_parse() {
        let mut store = Store::default();
        // The uri names no file, so a document coming back at all proves the
        // request was answered from the store rather than from disk.
        let uri = uri("/unopened/unchanged-buffer.spml");
        let opened = store.update(&uri, 1, TEXT.to_string()).expect("parses");

        let first = store.get_or_load(&uri).expect("is held");
        let second = store.get_or_load(&uri).expect("is held");

        assert!(Arc::ptr_eq(&opened, &first));
        assert!(Arc::ptr_eq(&opened, &second));
    }

    #[test]
    fn test_a_new_version_invalidates_the_parse() {
        let mut store = Store::default();
        let uri = uri("/unopened/new-version.spml");

        let first = store.update(&uri, 1, TEXT.to_string()).expect("parses");
        let second = store
            .update(&uri, 2, CHANGED_TEXT.to_string())
            .expect("parses");

        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(&*second.text, CHANGED_TEXT);
        assert_eq!(second.version, Some(2));
    }

    #[test]
    fn test_closing_releases_the_document() {
        let mut store = Store::default();
        let uri = uri("/unopened/closed.spml");
        store.update(&uri, 1, TEXT.to_string()).expect("parses");

        assert!(store.close(&uri));

        assert!(store.documents.is_empty());
        assert!(!store.close(&uri));
    }

    #[test]
    fn test_saving_without_text_retains_the_buffer() {
        let mut store = Store::default();
        // The uri names no file, so a document coming back at all proves the
        // save did not fall through to disk.
        let uri = uri("/unopened/saved-without-text.spml");
        let opened = store.update(&uri, 1, TEXT.to_string()).expect("parses");

        let saved = store.save(&uri, None).expect("retains the buffer");

        assert!(Arc::ptr_eq(&opened, &saved));
    }

    #[test]
    fn test_saving_the_clients_own_buffer_retains_the_parse() {
        let mut store = Store::default();
        let uri = uri("/unopened/saved-with-text.spml");
        let opened = store.update(&uri, 1, TEXT.to_string()).expect("parses");

        let saved = store
            .save(&uri, Some(TEXT.to_string()))
            .expect("retains the buffer");

        assert!(Arc::ptr_eq(&opened, &saved));
        assert_eq!(saved.version, Some(1));
    }

    #[test]
    fn test_saving_text_the_client_never_sent_replaces_the_buffer() {
        let mut store = Store::default();
        let uri = uri("/unopened/saved-foreign-text.spml");
        store.update(&uri, 1, TEXT.to_string()).expect("parses");

        let saved = store
            .save(&uri, Some(CHANGED_TEXT.to_string()))
            .expect("parses");

        assert_eq!(&*saved.text, CHANGED_TEXT);
        assert_eq!(saved.version, None);
    }

    #[test]
    fn test_saving_a_document_the_store_does_not_hold_reads_disk() {
        let path = env::temp_dir().join(format!("lspml-save-{}.spml", process::id()));
        fs::write(&path, TEXT).expect("test file should be writable");
        let mut store = Store::default();

        let saved = store.save(&uri(&path.to_string_lossy()), None);

        let _ = fs::remove_file(&path);
        let saved = saved.expect("reads from disk");
        assert_eq!(&*saved.text, TEXT);
        assert_eq!(saved.version, None);
    }

    #[test]
    fn test_a_missing_file_yields_no_document() {
        let mut store = Store::default();

        assert!(store.get_or_load(&uri("/unopened/missing.spml")).is_err());
    }

    #[test]
    fn test_a_document_carries_a_line_index_of_its_own_text() {
        let document = Document::new(TEXT.to_string(), Some(1)).expect("parses");

        // The "ä" is two bytes wide but one utf-16 code unit, so every offset
        // behind it differs between the two.
        assert_eq!(
            document.line_index.utf16_position(19),
            Position {
                line: 0,
                character: 18
            }
        );
    }
}

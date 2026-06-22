use simulacra_memory::{Chunker, FixedTokenChunker, JsonlChunker, MarkdownSectionChunker};
use simulacra_types::Locator;

fn tokenized_document(tokens: usize) -> String {
    (0..tokens)
        .map(|index| format!("token{index:03}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn text_bounds(locator: &Locator) -> (usize, usize) {
    match locator {
        Locator::Text {
            byte_start,
            byte_end,
        } => (*byte_start, *byte_end),
        other => panic!("expected Locator::Text, got {other:?}"),
    }
}

fn chunk_token_count(text: &str) -> usize {
    text.split_whitespace().count()
}

#[test]
fn markdown_section_chunker_splits_on_h1_h2_and_h3_headings() {
    let source = "# Top\nalpha\n## Child\nbeta\n### Leaf\ngamma\n";
    let chunker = MarkdownSectionChunker;
    let chunker: &dyn Chunker = &chunker;

    let chunks = chunker
        .chunk("/var/memory/self/notes.md", source.as_bytes())
        .unwrap();

    assert_eq!(chunks.len(), 3);
    assert!(chunks[0].text.starts_with("# Top"));
    assert!(chunks[1].text.starts_with("## Child"));
    assert!(chunks[2].text.starts_with("### Leaf"));
}

#[test]
fn markdown_section_chunker_preserves_section_boundaries() {
    let source = "# One\nalpha line 1\nalpha line 2\n## Two\nbeta line 1\nbeta line 2\n";
    let chunker = MarkdownSectionChunker;
    let chunker: &dyn Chunker = &chunker;

    let chunks = chunker
        .chunk("/var/memory/self/notes.md", source.as_bytes())
        .unwrap();

    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].text.contains("alpha line 1"));
    assert!(chunks[0].text.contains("alpha line 2"));
    assert!(!chunks[0].text.contains("## Two"));
    assert!(chunks[1].text.starts_with("## Two"));
    assert!(chunks[1].text.contains("beta line 2"));
}

#[test]
fn markdown_section_chunker_emits_text_locators_that_match_source_byte_ranges() {
    let source = "# One\nalpha\n## Two\nbeta\n";
    let chunker = MarkdownSectionChunker;
    let chunker: &dyn Chunker = &chunker;
    let chunks = chunker
        .chunk("/var/memory/self/notes.md", source.as_bytes())
        .unwrap();

    for chunk in chunks {
        let (byte_start, byte_end) = text_bounds(&chunk.locator);
        assert_eq!(
            &source.as_bytes()[byte_start..byte_end],
            chunk.text.as_bytes()
        );
    }
}

#[test]
fn fixed_token_chunker_emits_400_token_windows_with_50_token_overlap() {
    let source = tokenized_document(900);
    let chunker = FixedTokenChunker::default();
    let chunker: &dyn Chunker = &chunker;

    let chunks = chunker
        .chunk("/var/memory/self/tokens.txt", source.as_bytes())
        .unwrap();

    assert_eq!(chunks.len(), 3);
    assert_eq!(chunk_token_count(&chunks[0].text), 400);
    assert_eq!(chunk_token_count(&chunks[1].text), 400);
    assert_eq!(chunk_token_count(&chunks[2].text), 200);

    let first_tail = chunks[0]
        .text
        .split_whitespace()
        .skip(350)
        .collect::<Vec<_>>();
    let second_head = chunks[1]
        .text
        .split_whitespace()
        .take(50)
        .collect::<Vec<_>>();
    assert_eq!(first_tail, second_head);
}

#[test]
fn fixed_token_chunker_emits_text_locators() {
    let source = tokenized_document(25);
    let chunker = FixedTokenChunker::default();
    let chunker: &dyn Chunker = &chunker;
    let chunks = chunker
        .chunk("/var/memory/self/tokens.txt", source.as_bytes())
        .unwrap();

    assert!(
        chunks
            .iter()
            .all(|chunk| matches!(chunk.locator, Locator::Text { .. }))
    );
}

#[test]
fn jsonl_chunker_emits_one_chunk_per_non_empty_line_with_jsonl_line_locators() {
    let source = "{\"id\":1}\n\n{\"id\":2}\n  \n{\"id\":3}\n";
    let chunker = JsonlChunker;
    let chunker: &dyn Chunker = &chunker;

    let chunks = chunker
        .chunk("/var/memory/self/log.jsonl", source.as_bytes())
        .unwrap();

    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "{\"id\":1}");
    assert_eq!(chunks[1].text, "{\"id\":2}");
    assert_eq!(chunks[2].text, "{\"id\":3}");

    assert!(matches!(chunks[0].locator, Locator::JsonlLine { line: 0 }));
    assert!(matches!(chunks[1].locator, Locator::JsonlLine { line: 2 }));
    assert!(matches!(chunks[2].locator, Locator::JsonlLine { line: 4 }));
}

#[test]
fn jsonl_chunker_skips_empty_lines() {
    let source = "\n\n{\"id\":1}\n\n";
    let chunker = JsonlChunker;
    let chunker: &dyn Chunker = &chunker;

    let chunks = chunker
        .chunk("/var/memory/self/log.jsonl", source.as_bytes())
        .unwrap();

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "{\"id\":1}");
}

#[test]
fn chunker_names_expose_the_markdown_fixed_token_and_jsonl_identities() {
    let markdown = MarkdownSectionChunker;
    let fixed = FixedTokenChunker::default();
    let jsonl = JsonlChunker;

    assert_eq!((&markdown as &dyn Chunker).name(), "markdown-section");
    assert_eq!((&fixed as &dyn Chunker).name(), "fixed-token");
    assert_eq!((&jsonl as &dyn Chunker).name(), "jsonl-line");
}

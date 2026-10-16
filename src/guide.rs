//! `sss guide`: the two documents an agent driving `sss` reads, embedded in
//! the binary — the workflow guide and the matching protocol.

use std::io::{self, Write};

use clap::ValueEnum;
use serde::Serialize;

/// The workflow guide: `docs/llm-guide.md`.
pub const AGENT: &str = include_str!("../docs/llm-guide.md");

/// The matching protocol: `docs/matching-protocol.md`.
pub const MATCHING: &str = include_str!("../docs/matching-protocol.md");

/// Which document `sss guide` prints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// How to operate `sss` on a person's behalf, start to finish
    #[default]
    Agent,
    /// The item and answer documents and the rules for deciding a match
    Matching,
}

impl Topic {
    /// The document's text.
    pub fn text(self) -> &'static str {
        match self {
            Topic::Agent => AGENT,
            Topic::Matching => MATCHING,
        }
    }
}

/// What `sss guide` reports: the topic and its document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuideReport {
    pub topic: Topic,
    pub text: &'static str,
}

impl GuideReport {
    pub fn new(topic: Topic) -> Self {
        GuideReport {
            topic,
            text: topic.text(),
        }
    }

    /// The document, verbatim.
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(self.text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Kind;
    use crate::matching::llm::{self, BatchItem, Completeness};
    use crate::spotify::ids::IdKind;
    use crate::spotify::item::Offered;
    use serde_json::Value;

    /// The body of the `music-matcher` agent definition: everything after
    /// the line that closes its frontmatter.
    fn agent_body() -> &'static str {
        let def = llm::DEFINITION;
        let mut offset = 0;
        let mut fences = 0;
        for line in def.split_inclusive('\n') {
            offset += line.len();
            if line.trim_end() == "---" {
                fences += 1;
                if fences == 2 {
                    return &def[offset..];
                }
            }
        }
        panic!("agents/music-matcher.md has no closing frontmatter `---` line");
    }

    #[test]
    fn the_protocol_holds_the_matcher_rules_verbatim() {
        let body = agent_body();
        assert!(body.starts_with("You match Pandora records"), "{body}");
        assert!(
            MATCHING.contains(body),
            "docs/matching-protocol.md does not contain the body of \
             agents/music-matcher.md: paste everything after the agent file's \
             closing `---` line into the protocol's rules section, unchanged"
        );
    }

    /// The fenced blocks of `doc` whose info string is `lang`, in order.
    fn fenced(doc: &str, lang: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        // The open block: whether it is in `lang`, and its text so far.
        let mut current: Option<(bool, String)> = None;
        for line in doc.lines() {
            let fence = line.trim_start().strip_prefix("```");
            match (&mut current, fence) {
                (None, Some(info)) => current = Some((info.trim() == lang, String::new())),
                (Some((wanted, block)), Some(_)) => {
                    if *wanted {
                        blocks.push(std::mem::take(block));
                    }
                    current = None;
                }
                (Some((_, block)), None) => {
                    block.push_str(line);
                    block.push('\n');
                }
                (None, None) => {}
            }
        }
        assert!(current.is_none(), "unclosed code fence");
        blocks
    }

    #[test]
    fn every_json_block_parses() {
        for (name, doc) in [("llm-guide.md", AGENT), ("matching-protocol.md", MATCHING)] {
            let blocks = fenced(doc, "json");
            assert!(!blocks.is_empty(), "{name} has no json block");
            for block in blocks {
                if let Err(e) = serde_json::from_str::<Value>(&block) {
                    panic!("docs/{name}: a json block does not parse ({e}):\n{block}");
                }
            }
        }
    }

    #[test]
    fn every_link_is_a_full_https_url() {
        for (name, doc) in [("llm-guide.md", AGENT), ("matching-protocol.md", MATCHING)] {
            for (i, _) in doc.match_indices("](") {
                let target = &doc[i + 2..];
                let target = &target[..target.find(')').expect("closed link")];
                assert!(
                    target.starts_with("https://"),
                    "docs/{name}: link target {target:?} is not a full https:// URL"
                );
            }
        }
    }

    #[test]
    fn the_guide_names_every_error_kind() {
        for kind in Kind::ALL {
            let word = serde_json::to_value(kind).unwrap();
            let word = word.as_str().expect("a kind serializes as a string");
            assert!(
                AGENT.contains(&format!("`{word}`")),
                "docs/llm-guide.md does not name the error kind `{word}`"
            );
        }
    }

    /// A questions document's items, as `parse_decisions` checks against.
    fn batch_items(questions: &Value) -> Vec<BatchItem> {
        questions["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| BatchItem {
                item: i["item"].as_str().expect("item").to_owned(),
                kind: match i["kind"].as_str().expect("kind") {
                    "track" => IdKind::Track,
                    "artist" => IdKind::Artist,
                    "album" => IdKind::Album,
                    other => panic!("kind {other}"),
                },
                pandora: i["pandora"].clone(),
                candidates: i["candidates"]
                    .as_array()
                    .expect("candidates")
                    .iter()
                    .map(|c| Offered {
                        id: c["id"].as_str().expect("id").to_owned(),
                        name: c["name"].as_str().expect("name").to_owned(),
                        artists: Vec::new(),
                        album: None,
                        album_type: None,
                        release_date: None,
                        duration_ms: None,
                        explicit: None,
                        isrc: None,
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn the_protocol_example_answer_validates_against_its_questions() {
        let example = &MATCHING[MATCHING
            .find("\n## 4. Example")
            .expect("the protocol has an example section")..];
        let blocks = fenced(example, "json");
        assert_eq!(blocks.len(), 2, "the example is a questions and an answer");
        let questions: Value = serde_json::from_str(&blocks[0]).unwrap();
        let offered = batch_items(&questions);
        assert_eq!(offered.len(), 2);
        let decisions = llm::parse_decisions(&blocks[1], &offered, Completeness::Any, |id| {
            format!("{id} is not in the example")
        })
        .unwrap_or_else(|p| panic!("{p:?}"));
        assert_eq!(decisions.len(), 2);
        assert!(decisions.iter().any(|d| d.choice.is_none()));
        assert!(decisions.iter().any(|d| d.choice.is_some()));
    }

    #[test]
    fn the_report_carries_the_topic_and_the_document() {
        let report = GuideReport::new(Topic::default());
        assert_eq!(report.topic, Topic::Agent);
        let mut out = Vec::new();
        report.write_text(&mut out).unwrap();
        assert_eq!(out, AGENT.as_bytes());
        let value = serde_json::to_value(GuideReport::new(Topic::Matching)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"topic": "matching", "text": MATCHING})
        );
    }
}

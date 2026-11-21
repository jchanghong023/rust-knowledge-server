#![cfg(windows)]

// Linux's fixed /dev/shm root cannot be isolated by changing the child's profile.
use std::{
    fmt::Write as _,
    fs::{self, File},
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use axum::{Json, Router, routing::post};
use reqwest::{Client, RequestBuilder, Response, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::{net::TcpListener, time};

const DEADLINE: Duration = Duration::from_secs(30);
const LIBRARY: &str = "synthetic-quality";
const BILINGUAL_QUERY: &str = "XTAP 怎样重新连接测试通路？";
const BRIDGE: &str = "bridge the violet loop";
const PROCEDURE: &str = "dft/xtap-procedure.md";
const GLOSSARY: &str = "dft/xtap-glossary.md";
const MERGED: &str = "merged/checkout.md";
const MERGED_EVIDENCE: &str = "| Gate | Condition |\r\n| --- | --- |\r\n| Sable latch | Only after the sandbox probe succeeds |\r\n```text\r\nfixture_sable_latch --dry-run\r\n```\r\n";

const DOCUMENTS: &[(&str, &str)] = &[
    (
        PROCEDURE,
        "# Reconnect an experimental test access path\n\nOpen the selector, bridge the violet loop, and close the latch.\n",
    ),
    (
        GLOSSARY,
        "# XTAP glossary\n\nXTAP is a fictional label. The XTAP label is not a procedure.\n",
    ),
    (
        "dft/stuck.md",
        "# Stuck-at fault model\n\nTo select the stuck-at fault model for a constant-level check, hold the synthetic node at zero.\n",
    ),
    (
        "dft/transition.md",
        "# Transition fault model\n\nTo select the transition fault model for a timing check, sample the synthetic node twice.\n",
    ),
    (
        "dft/limit-table.md",
        "# Synthetic loop_limit worksheet\n\n| Parameter | Lane | State | Ceiling |\n| --- | --- | --- | --- |\n| loop_limit | amber | standby | 11 |\n| loop_limit | violet | active | 13 |\n| loop_limit | violet | standby | 17 |\n",
    ),
    (
        "dft/limit-prose.md",
        "# Notes on loop_limit\n\nThe loop_limit parameter is mentioned here for amber lane setup, not for selecting a ceiling.\n",
    ),
    (
        "wiki/iris/definition.md",
        "# Definition and return contract\n\n`iris_fixture::arm_loop` defines the loop result contract.\n\n```rust\npub fn arm_loop() -> LoopTicket { LoopTicket::Fixture }\n```\n",
    ),
    (
        "wiki/lotus/definition.md",
        "# Lotus contract\n\n`lotus_fixture::arm_loop` controls a different synthetic latch.\n\n```rust\npub fn arm_loop() -> LotusTicket { LotusTicket::Fixture }\n```\n",
    ),
    (
        "wiki/iris/near.md",
        "# Extended helper\n\n`iris_fixture::arm_loop2` is unrelated to the original helper.\n",
    ),
    (
        "wiki/iris-plus/definition.md",
        "# Neighbor directory\n\n`arm_loop` here returns an unrelated fixture.\n",
    ),
    (
        "wiki/iris/example.md",
        "# Calling example\n\nTo call arm_loop, construct the fixture before invoking the example.\n\n```rust\nlet ticket = iris_fixture::arm_loop();\n```\n",
    ),
    (
        "wiki/iris/mentions.md",
        "# Changelog references\n\narm_loop arm_loop arm_loop arm_loop arm_loop.\n",
    ),
    (
        "dft/fw2.md",
        "# Phaseguard fixture FW2\n\nEnable Phaseguard in FW2 while the synthetic latch is active.\n",
    ),
    (
        "dft/fw1.md",
        "# Phaseguard fixture FW1 (obsolete)\n\nDisable Phaseguard in FW1 while the synthetic latch is active.\n",
    ),
    (
        "dft/fault-codes.md",
        "# Fixture fault catalogue\n\nThe FXN family has a synthetic diagnostic `FXN-742` for an inactive latch.\n",
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Keyword,
    Hybrid,
    Reranked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    Evidence,
    SemanticOnly,
    ExactDefinition,
    RelatedOnly,
}

struct Case {
    label: &'static str,
    query: &'static str,
    scope: Option<&'static str>,
    path: &'static str,
    excerpt: &'static str,
    outranks: Option<&'static str>,
    expect: Expect,
}

const CASES: &[Case] = &[
    Case {
        label: "DFT bilingual procedure",
        query: BILINGUAL_QUERY,
        scope: Some("dft"),
        path: PROCEDURE,
        excerpt: "Open the selector, bridge the violet loop, and close the latch.",
        outranks: None,
        expect: Expect::SemanticOnly,
    },
    Case {
        label: "DFT conflicting fault models",
        query: "stuck-at fault model constant-level check",
        scope: Some("dft"),
        path: "dft/stuck.md",
        excerpt: "hold the synthetic node at zero",
        outranks: Some("dft/transition.md"),
        expect: Expect::Evidence,
    },
    Case {
        label: "DFT parameter table",
        query: "loop_limit violet standby ceiling",
        scope: Some("dft"),
        path: "dft/limit-table.md",
        excerpt: "| Parameter | Lane | State | Ceiling |\n| --- | --- | --- | --- |\n| loop_limit | amber | standby | 11 |\n| loop_limit | violet | active | 13 |\n| loop_limit | violet | standby | 17 |",
        outranks: Some("dft/limit-prose.md"),
        expect: Expect::Evidence,
    },
    Case {
        label: "Wiki qualified symbol",
        query: "`iris_fixture::arm_loop`",
        scope: None,
        path: "wiki/iris/definition.md",
        excerpt: "pub fn arm_loop() -> LoopTicket",
        outranks: None,
        expect: Expect::ExactDefinition,
    },
    Case {
        label: "Wiki scoped short symbol",
        query: "arm_loop",
        scope: Some("wiki/iris"),
        path: "wiki/iris/definition.md",
        excerpt: "pub fn arm_loop() -> LoopTicket",
        outranks: None,
        expect: Expect::ExactDefinition,
    },
    Case {
        label: "Wiki definition intent",
        query: "arm_loop definition return contract",
        scope: Some("wiki/iris"),
        path: "wiki/iris/definition.md",
        excerpt: "defines the loop result contract",
        outranks: None,
        expect: Expect::Evidence,
    },
    Case {
        label: "Wiki usage intent",
        query: "how to call arm_loop example",
        scope: Some("wiki/iris"),
        path: "wiki/iris/example.md",
        excerpt: "let ticket = iris_fixture::arm_loop();",
        outranks: None,
        expect: Expect::Evidence,
    },
    Case {
        label: "Merged Markdown near a chunk boundary",
        query: "fixture_sable_latch",
        scope: Some("merged"),
        path: MERGED,
        excerpt: MERGED_EVIDENCE,
        outranks: None,
        expect: Expect::Evidence,
    },
    Case {
        label: "Current vs obsolete firmware",
        query: "Phaseguard FW2 active",
        scope: Some("dft"),
        path: "dft/fw2.md",
        excerpt: "Enable Phaseguard in FW2",
        outranks: Some("dft/fw1.md"),
        expect: Expect::Evidence,
    },
    Case {
        label: "No exact answer for a nearby fault code",
        query: "FXN-741",
        scope: Some("dft"),
        path: "dft/fault-codes.md",
        excerpt: "`FXN-742`",
        outranks: None,
        expect: Expect::RelatedOnly,
    },
];

#[tokio::test]
async fn synthetic_search_quality_through_production_binary() {
    let mut merged = String::from(
        "# Synthetic notes\r\n\r\n## Checkout\r\n\r\nThe unrelated `fixture_sable_latch2` is in this earlier section.\r\n\r\n",
    );
    let section = merged.len();
    merged.push_str("## Checkout\r\n\r\n");
    let padding = "This line only pads the synthetic merge fixture.\r\n";
    while merged.len() - section + padding.len() + 2 < 17_950 {
        merged.push_str(padding);
    }
    // The table and command would cross 18,000 UTF-16 units if not moved together after the blank.
    merged.push_str("\r\n");
    merged.push_str(MERGED_EVIDENCE);
    let mut documents = DOCUMENTS.to_vec();
    documents.push((MERGED, &merged));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_url = format!("http://{}", listener.local_addr().unwrap());
    let model = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/embeddings", post(embeddings))
                .route("/v1/rerank", post(rerank)),
        )
        .await
        .unwrap();
    });

    for mode in [Mode::Keyword, Mode::Hybrid, Mode::Reranked] {
        let server = Server::start(mode, &model_url).await;
        let cookie = server.import(&documents).await;
        for case in CASES {
            let reply = server.search(&cookie, case).await;
            case.verify(mode, &reply, &documents);
        }
    }
    model.abort();
}

async fn embeddings(Json(body): Json<Value>) -> Json<Value> {
    // A fixed cross-language feature; all other documents are tied on the dense branch.
    let data: Vec<_> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let text = text.as_str().unwrap();
            let feature = text == BILINGUAL_QUERY || text.contains(BRIDGE);
            json!({"index": index, "embedding": if feature { [1, 0] } else { [0, 1] }})
        })
        .collect();
    Json(json!({"data": data}))
}

async fn rerank(Json(body): Json<Value>) -> Json<Value> {
    let results: Vec<_> = body["documents"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let relevant =
                body["query"] == BILINGUAL_QUERY && text.as_str().unwrap().contains(BRIDGE);
            json!({"index": index, "relevance_score": i32::from(relevant)})
        })
        .collect();
    Json(json!({"results": results}))
}

#[derive(Debug, Deserialize)]
struct Hit {
    path: String,
    text: String,
    line_start: usize,
    line_end: usize,
    sha256: String,
    category: String,
}

#[derive(Deserialize)]
struct Reply {
    status: String,
    retrieval: String,
    reranked: bool,
    matches: Vec<Hit>,
}

impl Case {
    fn verify(&self, mode: Mode, reply: &Reply, documents: &[(&str, &str)]) {
        let context = format!("{mode:?} / {}", self.label);
        let hits = &reply.matches;
        assert_eq!(reply.status, "ok", "{context}");
        let retrieval = if mode == Mode::Keyword {
            "keyword"
        } else {
            "hybrid"
        };
        assert_eq!(reply.retrieval, retrieval, "{context}");
        assert_eq!(reply.reranked, mode == Mode::Reranked, "{context}");
        if let Some(scope) = self.scope {
            assert!(
                hits.iter().all(|hit| hit
                    .path
                    .strip_prefix(scope)
                    .is_some_and(|rest| rest.starts_with('/'))),
                "{context}: returned a result outside {scope}"
            );
        }

        let (path, excerpt) = if self.expect == Expect::SemanticOnly && mode == Mode::Keyword {
            assert!(
                !hits.iter().any(|hit| hit.path == self.path),
                "{context}: procedure unexpectedly matched keywords"
            );
            (GLOSSARY, "XTAP is a fictional label")
        } else {
            (self.path, self.excerpt)
        };
        let rank = hits
            .iter()
            .take(5)
            .position(|hit| hit.path == path && hit.text.contains(excerpt))
            .unwrap_or_else(|| {
                let returned: Vec<_> = hits
                    .iter()
                    .map(|hit| (&hit.path, hit.line_start, hit.line_end))
                    .collect();
                panic!("{context}: missing {path} evidence in Top 5; returned {returned:?}");
            });
        let hit = &hits[rank];
        match self.expect {
            Expect::SemanticOnly if mode == Mode::Reranked => assert_eq!(rank, 0, "{context}"),
            Expect::ExactDefinition => {
                assert_eq!(hit.category, "exact", "{context}");
                let near = [
                    "wiki/lotus/definition.md",
                    "wiki/iris/near.md",
                    "wiki/iris-plus/definition.md",
                ];
                assert!(
                    !hits
                        .iter()
                        .any(|hit| hit.category == "exact" && near.contains(&hit.path.as_str())),
                    "{context}: another symbol was treated as exact"
                );
            }
            Expect::RelatedOnly => {
                assert!(
                    hits.iter().all(|hit| hit.category == "related"),
                    "{context}: an absent code was treated as exact"
                );
            }
            _ => {}
        }
        if let Some(decoy) = self.outranks {
            let decoy_rank = hits
                .iter()
                .position(|hit| hit.path == decoy)
                .unwrap_or_else(|| panic!("{context}: missing competing result {decoy}"));
            assert!(
                rank < decoy_rank,
                "{context}: {decoy} outranked the evidence"
            );
        }

        let source = documents.iter().find(|(name, _)| *name == path).unwrap().1;
        assert!(
            hit.line_start > 0 && hit.line_end >= hit.line_start,
            "{context}"
        );
        let mut lines = source.split_inclusive('\n').skip(hit.line_start - 1);
        let mut original = String::new();
        for _ in hit.line_start..=hit.line_end {
            original.push_str(lines.next().expect(&context));
        }
        assert_eq!(
            hit.text, original,
            "{context}: evidence did not match the original line range"
        );
        let sha256: String = Sha256::digest(source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(hit.sha256, sha256, "{context}");
        println!(
            "{context}: #{} {path}:{}-{}",
            rank + 1,
            hit.line_start,
            hit.line_end
        );
    }
}

struct ChildProcess(Child);

impl Drop for ChildProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Server {
    _process: ChildProcess,
    _home: TempDir,
    origin: String,
    client: Client,
}

impl Server {
    async fn start(mode: Mode, model_url: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home
            .path()
            .canonicalize()
            .unwrap()
            .join(".rust-knowledge-server");
        let startup = home.path().join("startup.log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_rust-knowledge-server"));
        command
            .env("USERPROFILE", home.path())
            .env_remove("OMP_OFFLINE")
            .args(["--listen", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(File::create(&startup).unwrap());
        if mode != Mode::Keyword {
            command.args([
                "--embedding-url",
                model_url,
                "--embedding-model",
                "synthetic",
                "--embedding-dimensions",
                "2",
            ]);
        }
        if mode == Mode::Reranked {
            command.args(["--rerank-url", model_url, "--rerank-model", "synthetic"]);
        }
        let mut process = ChildProcess(command.spawn().unwrap());
        let origin = time::timeout(DEADLINE, async {
            loop {
                let bytes = fs::read(&startup).unwrap();
                let output = String::from_utf8_lossy(&bytes);
                let mut lines = output
                    .split_inclusive('\n')
                    .filter(|line| line.ends_with('\n'))
                    .map(str::trim_end);
                let data_root = lines.find_map(|line| line.strip_prefix("Data root: "));
                if let Some(address) =
                    lines.find_map(|line| line.strip_prefix("Listening on http://"))
                {
                    let data_root =
                        data_root.expect("production binary did not report its data root");
                    assert_eq!(Path::new(data_root), root);
                    let address: SocketAddr = address.parse().unwrap();
                    assert!(address.ip().is_loopback());
                    break format!("http://{address}");
                }
                assert_eq!(
                    process.0.try_wait().unwrap(),
                    None,
                    "server exited during startup: {output}"
                );
                time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("production binary did not start");
        Self {
            _process: process,
            _home: home,
            origin,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(DEADLINE)
                .build()
                .unwrap(),
        }
    }

    fn post(&self, path: &str, cookie: &str) -> RequestBuilder {
        let request = self
            .client
            .post(format!("{}{path}", self.origin))
            .header(header::ORIGIN, &self.origin);
        if cookie.is_empty() {
            request
        } else {
            request.header(header::COOKIE, cookie)
        }
    }

    async fn import(&self, documents: &[(&str, &str)]) -> String {
        let response = send(
            self.post("/api/session", "")
                .json(&json!({"employee": "00001234"})),
            StatusCode::OK,
        )
        .await;
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        send(
            self.post("/api/libraries", &cookie)
                .json(&json!({"name": LIBRARY})),
            StatusCode::CREATED,
        )
        .await;

        const BOUNDARY: &str = "synthetic-search-quality-boundary";
        let mut upload = String::new();
        for (path, source) in documents {
            write!(upload, "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\n{path}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload.md\"\r\nContent-Type: text/markdown\r\n\r\n{source}\r\n").unwrap();
        }
        write!(upload, "--{BOUNDARY}--\r\n").unwrap();
        let accepted: Value = send(
            self.post("/api/library/import", &cookie)
                .query(&[("name", LIBRARY)])
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(upload),
            StatusCode::ACCEPTED,
        )
        .await
        .json()
        .await
        .unwrap();
        let id = accepted["id"].as_str().unwrap();
        time::timeout(DEADLINE, async {
            loop {
                let request = self
                    .client
                    .get(format!("{}/api/operations/{id}", self.origin))
                    .header(header::COOKIE, &cookie);
                let status: Value = send(request, StatusCode::OK).await.json().await.unwrap();
                match status["outcome"].as_str() {
                    Some("succeeded") => break,
                    Some("in_progress") => time::sleep(Duration::from_millis(20)).await,
                    _ => panic!("synthetic import failed: {status}"),
                }
            }
        })
        .await
        .expect("synthetic import did not complete");
        cookie
    }

    async fn search(&self, cookie: &str, case: &Case) -> Reply {
        send(
            self.post("/api/search", cookie).json(&json!({
                "query": case.query,
                "library": LIBRARY,
                "path": case.scope,
            })),
            StatusCode::OK,
        )
        .await
        .json()
        .await
        .unwrap()
    }
}

async fn send(request: RequestBuilder, status: StatusCode) -> Response {
    let response = request.send().await.unwrap();
    let actual = response.status();
    if actual != status {
        let url = response.url().clone();
        panic!(
            "expected {status}; {url} returned {actual}: {}",
            response.text().await.unwrap()
        );
    }
    response
}

use std::fs;

use axum::http::header;
use reqwest::{Client, Response as HttpResponse};
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time,
};

use super::*;
use crate::data_root::Root;

const ORIGIN: &str = "https://workbench.invalid";

struct Fixture {
    client: Client,
    address: String,
    server: JoinHandle<()>,
    root: tempfile::TempDir,
}

impl Fixture {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let catalog = Arc::new(Catalog::open(Root::open(root.path()).unwrap()).unwrap());
        let audit = Arc::new(Audit::open(catalog.root_path(), &catalog).unwrap());
        let app = router(
            catalog,
            audit,
            None,
            None,
            ORIGIN.into(),
            TaskTracker::new(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            client: Client::builder().no_proxy().build().unwrap(),
            address,
            server,
            root,
        }
    }

    fn url(&self, route: &str) -> String {
        format!("{}{route}", self.address)
    }

    async fn register(&self) -> String {
        let response = self
            .client
            .post(self.url("/api/session"))
            .header(header::ORIGIN, ORIGIN)
            .json(&json!({"employee": "00001234"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        for flag in ["Path=/", "HttpOnly", "SameSite=Strict", "Secure"] {
            assert!(
                cookie.split("; ").any(|part| part == flag),
                "missing cookie flag {flag}"
            );
        }
        let token = cookie.split_once(';').unwrap().0.to_owned();
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"employee": "00001234"})
        );
        token
    }

    fn count_starts(&self) -> usize {
        fs::read_dir(self.root.path().join("audit"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_str().unwrap().ends_with(".start.json"))
            .count()
    }

    async fn operation(&self, response: HttpResponse, cookie: &str) -> Value {
        let id = response.headers()["x-operation-id"].to_str().unwrap();
        let status = self
            .client
            .get(self.url(&format!("/api/operations/{id}")))
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        status.json().await.unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn origin_and_session_ambiguity_is_rejected_before_audit_begin() {
    let fixture = Fixture::start().await;
    let cookie = fixture.register().await;
    let create = || {
        fixture
            .client
            .post(fixture.url("/api/libraries"))
            .json(&json!({"name": "docs"}))
    };
    let proxy_only = create()
        .header(header::COOKIE, &cookie)
        .header("x-forwarded-host", "workbench.invalid")
        .header("x-forwarded-proto", "https")
        .send()
        .await
        .unwrap();
    assert_eq!(proxy_only.status(), StatusCode::FORBIDDEN);
    let duplicate_origin = create()
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::ORIGIN, ORIGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate_origin.status(), StatusCode::FORBIDDEN);
    let duplicate_cookie = create()
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, format!("{cookie}; {cookie}"))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate_cookie.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fixture.count_starts(), 0);
}

#[tokio::test]
async fn import_extraction_and_validation_do_not_publish_or_change_the_actor() {
    let fixture = Fixture::start().await;
    let cookie = fixture.register().await;
    let created = fixture
        .client
        .post(fixture.url("/api/libraries"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, &cookie)
        .json(&json!({"name": ".."}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let status = fixture.operation(created, &cookie).await;
    assert_eq!(status["outcome"], "succeeded");
    assert_eq!(fixture.count_starts(), 1);

    let upload = || {
        fixture
            .client
            .post(fixture.url("/api/library/import?name=.."))
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, &cookie)
    };
    let malformed = upload()
        .header(header::CONTENT_TYPE, "multipart/form-data")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.count_starts(), 1);

    let injected = upload().header(header::CONTENT_TYPE, "multipart/form-data; boundary=test-edge")
        .body(concat!(
            "--test-edge\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\ndoc.md\r\n",
            "--test-edge\r\nContent-Disposition: form-data; name=\"file\"; filename=\"doc.md\"\r\n\r\n# valid file\r\n",
            "--test-edge\r\nContent-Disposition: form-data; name=\"employee\"\r\n\r\n99999999\r\n",
            "--test-edge--\r\n",
        ))
        .send().await.unwrap();
    assert_eq!(injected.status(), StatusCode::BAD_REQUEST);
    let status = fixture.operation(injected, &cookie).await;
    assert_eq!(status["outcome"], "failed");
    assert_eq!(status["employee"], "00001234");
    assert_eq!(status["employee_self_reported"], true);
    assert_eq!(status["library"], "..");
    let libraries = fixture
        .client
        .get(fixture.url("/api/libraries"))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        libraries.json::<Value>().await.unwrap(),
        json!([{"name": "..", "published": false}])
    );
}

#[tokio::test]
async fn saturated_import_records_the_rejection_without_starting_a_build() {
    let fixture = Fixture::start().await;
    let cookie = fixture.register().await;
    let response = fixture
        .client
        .post(fixture.url("/api/libraries"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, &cookie)
        .json(&json!({"name": "docs"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Two clients pause mid-upload, keeping both permits without starting
    // Edge. A third upload must receive an attributable refusal.
    let address = fixture.address.strip_prefix("http://").unwrap();
    let mut uploads = Vec::new();
    for _ in 0..2 {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!(
                    "POST /api/library/import?name=docs HTTP/1.1\r\nHost: {address}\r\n\
             Origin: {ORIGIN}\r\nCookie: {cookie}\r\nContent-Length: 1000\r\n\
             Content-Type: multipart/form-data; boundary=test-edge\r\n\r\n--test-edge\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        uploads.push(stream);
    }
    let versions = fixture.root.path().join("versions");
    time::timeout(Duration::from_secs(10), async {
        while fs::read_dir(&versions).unwrap().count() != 2 {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let response = fixture
        .client
        .post(fixture.url("/api/library/import?name=docs"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, &cookie)
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=test-edge",
        )
        .body("--test-edge--\r\n")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let status = fixture.operation(response, &cookie).await;
    assert_eq!(status["outcome"], "failed");
    assert_eq!(status["action"], "import");
    assert_eq!(status["library"], "docs");
    assert_eq!(status["employee"], "00001234");
    assert_eq!(fs::read_dir(versions).unwrap().count(), 2);
    drop(uploads);
}

#[tokio::test]
async fn name_at_the_utf8_limit_is_manageable_and_an_extra_byte_is_rejected() {
    let fixture = Fixture::start().await;
    let cookie = fixture.register().await;
    let name = format!("{}x", "锚".repeat(341));
    for (name, expected) in [
        (format!("{name}x"), StatusCode::BAD_REQUEST),
        (name.clone(), StatusCode::CREATED),
    ] {
        let response = fixture
            .client
            .post(fixture.url("/api/libraries"))
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, &cookie)
            .json(&json!({"name": name}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let response = fixture
        .client
        .delete(fixture.url("/api/library"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, &cookie)
        .query(&[("name", name)])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn multipart_stream_limit_remains_a_413_not_a_malformed_upload() {
    use axum::{body::Body, extract::FromRequest, http::Request};

    let root = tempfile::tempdir().unwrap();
    let mut request = Request::post("/api/library/import")
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=test-edge",
        )
        .body(Body::from(concat!(
            "--test-edge\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\ndoc.md\r\n",
            "--test-edge\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n# above limit\r\n",
            "--test-edge--\r\n",
        )))
        .unwrap();
    DefaultBodyLimit::max(128).apply(&mut request);
    let multipart = Multipart::from_request(request, &()).await.unwrap();
    let Err(error) = import::stage(multipart, root.path()).await else {
        panic!("multipart above the request body limit was accepted");
    };
    assert_eq!(
        ApiError::from(error).into_response().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(root.path().read_dir().unwrap().next().is_none());
}

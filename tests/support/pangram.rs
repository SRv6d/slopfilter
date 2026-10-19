use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use url::Url;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub(crate) struct Response {
    status: u16,
    body: String,
}

impl Response {
    pub(crate) fn ok(body: impl Into<String>) -> Self {
        Self::json(200, body)
    }

    pub(crate) fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

#[derive(Clone)]
struct ScriptedResponder {
    responses: Arc<Mutex<VecDeque<Response>>>,
}

impl Respond for ScriptedResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let Some(response) = self
            .responses
            .lock()
            .expect("the scripted Pangram response lock is not poisoned")
            .pop_front()
        else {
            return ResponseTemplate::new(500)
                .set_body_string("the Pangram test server received an unexpected request");
        };

        ResponseTemplate::new(response.status)
            .insert_header("content-type", "application/json")
            .set_body_string(response.body)
    }
}

pub(crate) struct ScriptedServer {
    server: MockServer,
    responses: Arc<Mutex<VecDeque<Response>>>,
    expected_requests: usize,
}

impl ScriptedServer {
    pub(crate) async fn start(responses: impl IntoIterator<Item = Response>) -> Self {
        let responses = Arc::new(Mutex::new(responses.into_iter().collect::<VecDeque<_>>()));
        let expected_requests = responses
            .lock()
            .expect("the scripted Pangram response lock is not poisoned")
            .len();
        let server = MockServer::start().await;

        Mock::given(|_request: &Request| true)
            .respond_with(ScriptedResponder {
                responses: Arc::clone(&responses),
            })
            .mount(&server)
            .await;

        Self {
            server,
            responses,
            expected_requests,
        }
    }

    pub(crate) fn base_url(&self) -> Url {
        Url::parse(&format!("{}/", self.server.uri()))
            .expect("wiremock provides a valid HTTP server URL")
    }

    pub(crate) async fn finish(&self) -> Vec<Request> {
        let remaining = self
            .responses
            .lock()
            .expect("the scripted Pangram response lock is not poisoned")
            .len();
        assert_eq!(remaining, 0, "not all scripted Pangram responses were used");

        let requests = self
            .server
            .received_requests()
            .await
            .expect("request recording is enabled for the Pangram test server");
        assert_eq!(
            requests.len(),
            self.expected_requests,
            "the Pangram test server received an unexpected number of requests"
        );
        requests
    }
}

pub(crate) fn assert_api_key(requests: &[Request], expected: &str) {
    for request in requests {
        assert_eq!(
            request
                .headers
                .get("x-api-key")
                .expect("request includes the Pangram API key")
                .to_str()
                .unwrap(),
            expected
        );
    }
}

pub(crate) fn assert_request(request: &Request, method: &str, path: &str) {
    assert_eq!(request.method.as_str(), method);
    assert_eq!(request.url.path(), path);
}

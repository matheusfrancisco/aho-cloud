//! `FakeRunMcpHost`: records the `RunTools` each run started its Run MCP with, so session tests
//! drive tools directly (`started()[0].call(..)`), and counts shutdowns. No sockets.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::app::runner::tools::RunTools;
use crate::ports::run_mcp::{RunMcpEndpoint, RunMcpError, RunMcpHandle, RunMcpHost};

#[derive(Default)]
pub struct FakeRunMcpHost {
    started: parking_lot::Mutex<Vec<Arc<RunTools>>>,
    fail_next: parking_lot::Mutex<Option<RunMcpError>>,
    shutdowns: Arc<AtomicUsize>,
}

impl FakeRunMcpHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next `start` fails with `err`.
    pub fn fail_next(&self, err: RunMcpError) {
        *self.fail_next.lock() = Some(err);
    }

    pub fn started(&self) -> Vec<Arc<RunTools>> {
        self.started.lock().clone()
    }

    pub fn shutdowns(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RunMcpHost for FakeRunMcpHost {
    async fn start(&self, tools: Arc<RunTools>) -> Result<Box<dyn RunMcpHandle>, RunMcpError> {
        if let Some(err) = self.fail_next.lock().take() {
            return Err(err);
        }
        let n = {
            let mut started = self.started.lock();
            started.push(tools.clone());
            started.len()
        };
        Ok(Box::new(FakeRunMcpHandle {
            endpoint: RunMcpEndpoint { url: format!("http://127.0.0.1:{}/mcp", 40000 + n), bearer: format!("fake-run-mcp-bearer-{n:04}") },
            tools,
            shutdowns: self.shutdowns.clone(),
            done: parking_lot::Mutex::new(false),
        }))
    }
}

struct FakeRunMcpHandle {
    endpoint: RunMcpEndpoint,
    tools: Arc<RunTools>,
    shutdowns: Arc<AtomicUsize>,
    done: parking_lot::Mutex<bool>,
}

#[async_trait]
impl RunMcpHandle for FakeRunMcpHandle {
    fn endpoint(&self) -> &RunMcpEndpoint {
        &self.endpoint
    }

    async fn shutdown(&self) {
        let mut done = self.done.lock();
        if !*done {
            *done = true;
            self.tools.shutdown();
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
    }
}

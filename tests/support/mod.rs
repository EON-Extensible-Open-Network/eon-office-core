// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Test support: a real core process on a loopback port, and a client that
//! speaks the protocol with nothing but a socket and `serde_json` -- on
//! purpose not the SDK, so these tests prove the wire format, not a library.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

pub fn fresh_home() -> PathBuf {
    let n = NEXT_HOME.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "eon-office-test-{}-{n}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

pub fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_eon-office-core")
}

pub struct CoreProcess {
    pub child: Option<Child>,
    pub home: PathBuf,
    pub address: String,
    pub owner_token: String,
    keep_home: bool,
}

impl CoreProcess {
    pub fn start() -> Self {
        Self::start_in(fresh_home(), false)
    }

    pub fn start_in(home: PathBuf, keep_home: bool) -> Self {
        let _ = std::fs::remove_file(home.join("runtime").join("endpoint.json"));
        let child = Command::new(binary())
            .args(["serve", "--home"])
            .arg(&home)
            .args(["--listen", "127.0.0.1:0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the core");
        let endpoint = home.join("runtime").join("endpoint.json");
        let deadline = Instant::now() + Duration::from_secs(20);
        let address = loop {
            if let Ok(text) = std::fs::read_to_string(&endpoint) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(address) = value["address"].as_str() {
                        break address.to_owned();
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "the core did not write endpoint.json"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        let owner_token = std::fs::read_to_string(home.join("runtime").join("owner.token"))
            .unwrap()
            .trim()
            .to_owned();
        CoreProcess {
            child: Some(child),
            home,
            address,
            owner_token,
            keep_home,
        }
    }

    pub fn connect(&self) -> Client {
        Client::connect(&self.address)
    }

    pub fn owner(&self) -> Client {
        let mut client = self.connect();
        client
            .call(
                "session.hello",
                json!({ "token": self.owner_token, "api": 0 }),
            )
            .expect("owner hello");
        client
    }

    /// Install `manifest`, granting `grant`; returns the app's token.
    pub fn install(&self, owner: &mut Client, manifest: Value, grant: &[&str]) -> String {
        let id = manifest["id"].as_str().unwrap().to_owned();
        owner
            .call(
                "apps.install",
                json!({ "manifest": manifest, "grant": grant }),
            )
            .expect("install");
        std::fs::read_to_string(self.home.join("apps").join(&id).join("token"))
            .unwrap()
            .trim()
            .to_owned()
    }

    pub fn app(&self, id: &str, token: &str) -> Client {
        let mut client = self.connect();
        client
            .call("session.hello", json!({ "app": id, "token": token }))
            .expect("app hello");
        client
    }

    /// Wait for the process to exit by itself.
    pub fn wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("the core did not stop");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

impl Drop for CoreProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !self.keep_home {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}

pub fn manifest(id: &str, capabilities: &[&str]) -> Value {
    json!({
        "manifest": 0,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "api": { "min": 0, "max": 0 },
        "capabilities": capabilities,
    })
}

pub struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next: u64,
    pub inbox: VecDeque<Value>,
}

impl Client {
    pub fn connect(address: &str) -> Self {
        let stream = TcpStream::connect(address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next: 1,
            inbox: VecDeque::new(),
        }
    }

    pub fn send_raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    pub fn send(&mut self, value: &Value) {
        self.send_raw(&value.to_string());
    }

    /// Read one message; `None` on timeout or when the core closed the connection.
    pub fn read(&mut self, timeout: Duration) -> Option<Value> {
        self.writer.set_read_timeout(Some(timeout)).unwrap();
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(&line).expect("the core sent JSON")),
        }
    }

    /// Call and wait for the answer; anything else that arrives is kept in `inbox`.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next;
        self.next += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let message = self
                .read(Duration::from_secs(15))
                .unwrap_or_else(|| panic!("no answer to {method}"));
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message["result"].clone()),
                };
            }
            self.inbox.push_back(message);
        }
    }

    /// The next message that is not a reply: an event, `invoke`, `job.due`, `shutdown`.
    pub fn next_message(&mut self, timeout: Duration) -> Option<Value> {
        if let Some(message) = self.inbox.pop_front() {
            return Some(message);
        }
        self.read(timeout)
    }

    /// Wait for a message satisfying `want`, keeping the others.
    pub fn wait_for(&mut self, timeout: Duration, want: impl Fn(&Value) -> bool) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        if let Some(pos) = self.inbox.iter().position(&want) {
            return self.inbox.remove(pos);
        }
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.read(left.max(Duration::from_millis(10))) {
                Some(message) if want(&message) => return Some(message),
                Some(message) => self.inbox.push_back(message),
                None => return None,
            }
        }
        None
    }

    pub fn answer(&mut self, id: &Value, result: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn close(self) {
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
    }
}

pub fn code(error: &Value) -> &str {
    error["data"]["code"].as_str().unwrap_or("")
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}

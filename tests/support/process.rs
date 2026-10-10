//! Real-binary test harness. All state, keys and logs live in a temporary directory.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct ServerProcess {
    pub child: Child,
    pub addr: SocketAddr,
}

impl ServerProcess {
    pub fn spawn(binary: &str, dir: &Path, port_env: &str, env: &[(&str, String)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let log = std::fs::File::create(dir.join("server.log")).unwrap();
        let child = Command::new(binary)
            .env_clear()
            .env(port_env, addr.port().to_string())
            .envs(env.iter().map(|(k, v)| (*k, v)))
            .current_dir(dir)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        Self { child, addr }
    }

    pub fn wait_for_health(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(response) = self.request("/health", &[])
                && response.starts_with("HTTP/1.1 200")
            {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "server exited before listening"
            );
            assert!(Instant::now() < deadline, "server did not become healthy");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "server did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn request(&self, path: &str, headers: &[(&str, &str)]) -> std::io::Result<String> {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.addr
        );
        for (key, value) in headers {
            request.push_str(&format!("{key}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes())?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

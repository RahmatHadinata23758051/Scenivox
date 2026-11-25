use std::{
    fs::{self, File},
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use scenivox::db;

struct Server {
    child: Child,
    directory: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn clean_startup_creates_database_with_defaults_and_example_env() {
    // Each child gets its own environment and working directory; no test mutates
    // the parent process environment or depends on a developer's existing DB.
    for with_example in [false, true] {
        let directory = std::env::temp_dir().join(format!(
            "scenivox-startup-{}-{}-{with_example}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir(&directory).unwrap();
        if with_example {
            fs::write(directory.join(".env"), include_str!("../.env.example")).unwrap();
        }
        assert!(!directory.join("scenivox.db").exists());
        let log_path = directory.join("startup.log");
        let output = File::create(&log_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_scenivox"))
            .arg("serve")
            .current_dir(&directory)
            .env_remove("SCENIVOX_DATABASE_URL")
            .env_remove("RUST_LOG")
            // Let the OS choose a free port; this also checks environment
            // overrides take precedence over the example .env file.
            .env("SCENIVOX_ADDR", "127.0.0.1:0")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .env("NO_COLOR", "1")
            .stderr(Stdio::from(output))
            .spawn()
            .expect("start backend");
        let mut server = Server { child, directory };
        let deadline = Instant::now() + Duration::from_secs(15);
        let address = loop {
            let log = fs::read_to_string(&log_path).unwrap();
            if let Some((_, address)) = log.split_once("address=") {
                break address
                    .trim()
                    .parse::<std::net::SocketAddr>()
                    .expect("listening address");
            }
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "startup exited: {log}"
            );
            assert!(Instant::now() < deadline, "startup timeout: {log}");
            std::thread::sleep(Duration::from_millis(25));
        };
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.ends_with("{\"status\":\"ok\"}"), "{response}");
        assert!(server.directory.join("scenivox.db").is_file());
        server.child.kill().unwrap();
        server.child.wait().unwrap();

        let url = format!(
            "sqlite://{}",
            server
                .directory
                .join("scenivox.db")
                .to_string_lossy()
                .replace('\\', "/")
        );
        let pool = db::connect(&url)
            .await
            .expect("reconnect to startup database");
        let migrations: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(migrations, 10);
        // The migrated table must be writable, not merely present in metadata.
        sqlx::query("INSERT INTO app_metadata (key, value) VALUES ('startup', 'verified')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }
}

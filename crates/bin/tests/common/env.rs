use std::{
    env,
    process::{Child, Command, Stdio},
    thread,
};

#[derive(Debug)]
pub(crate) struct DhcpServerEnv {
    daemon: Child,
    db_name: String,
    netns: String,
    veth_cli: String,
    // veth_srv: String,
    // srv_ip: String,
}

impl DhcpServerEnv {
    pub(crate) fn start(
        config: &str,
        db: &str,
        netns: &str,
        veth_cli: &str,
        veth_srv: &str,
        srv_ip: &str,
    ) -> Self {
        // Clean up any leftover resources from previous failed tests
        // This is necessary because if start() panics before returning Self,
        // Drop won't run and resources won't be cleaned up
        remove_test_veth_nics(veth_cli);
        remove_test_net_namespace(netns);

        // dora is Postgres-only, so provision a transient database for this test.
        // The harness creates it over the admin `DATABASE_URL` (localhost), while
        // the dora process — which runs inside the network namespace and cannot
        // reach the host's loopback — connects to the same server over the veth at
        // `DORA_TEST_DB_HOST` (the host side of the veth pair). Both addresses hit
        // the same Postgres, so the database created here is visible to dora.
        let db_name = pg_db_name(db);
        block_on(ip_manager::postgres::create_test_database(&db_name))
            .expect("failed to create test database");
        let db_url = pg_url_for(&db_name);

        create_test_net_namespace(netns);
        create_test_veth_nics(netns, srv_ip, veth_cli, veth_srv);
        Self {
            daemon: start_dhcp_server(config, netns, &db_url),
            db_name,
            netns: netns.to_owned(),
            veth_cli: veth_cli.to_owned(),
            // veth_srv: veth_srv.to_owned(),
            // srv_ip: srv_ip.to_owned(),
        }
    }
}

impl Drop for DhcpServerEnv {
    fn drop(&mut self) {
        stop_dhcp_server(&mut self.daemon);
        remove_test_veth_nics(&self.veth_cli);
        remove_test_net_namespace(&self.netns);
        // best-effort: the daemon is dead, so its connection is gone; WITH (FORCE)
        // in drop_test_database evicts any that lingers.
        if let Err(err) = block_on(ip_manager::postgres::drop_test_database(&self.db_name)) {
            eprintln!("failed to drop test database {}: {err:?}", self.db_name);
        }
    }
}

/// Run a future to completion on a throwaway current-thread runtime (the harness
/// itself is synchronous; only the DB provisioning is async).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime")
        .block_on(fut)
}

/// Derive a valid Postgres identifier from the test's legacy `.db` filename,
/// e.g. `basic.db` -> `dora_it_basic_db`. Non-alphanumeric bytes become `_`.
///
/// The name is deterministic (not per-run unique), so two dora-bin tests using
/// the same config filename share a database name. That is safe only because the
/// dora-bin package is pinned to nextest's single-threaded `serial-integration`
/// group (see .config/nextest.toml) and `start()` drops+recreates the database;
/// concurrent runs would otherwise clobber each other.
fn pg_db_name(db: &str) -> String {
    let sanitized: String = db
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("dora_it_{}", sanitized.to_ascii_lowercase())
}

/// Build the connection URL dora (inside the netns) uses to reach Postgres: take
/// the admin `DATABASE_URL`'s credentials but point at `DORA_TEST_DB_HOST` (the
/// host end of the veth, reachable from the namespace) and the per-test database.
///
/// In CI the `postgres:16` service publishes `5432`, so Docker's DNAT accepts the
/// connection on the veth host IP. For a LOCAL run, Postgres must listen on (and
/// accept from) `DORA_TEST_DB_HOST` — a stock `localhost`-only Postgres will
/// refuse dora's connection even though the harness creates the DB fine over
/// localhost. Set `DORA_TEST_DB_HOST` to a reachable address if 192.168.2.99 is
/// not bound.
fn pg_url_for(db_name: &str) -> String {
    let base = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dora:dora@localhost:5432/dora".to_string());
    // credentials portion, i.e. everything before the '@' host segment
    let creds = base.split('@').next().unwrap_or("postgres://dora:dora");
    let host = env::var("DORA_TEST_DB_HOST").unwrap_or_else(|_| "192.168.2.99".to_string());
    format!("{creds}@{host}/{db_name}")
}

const SUDO: &str = "sudo";

fn create_test_net_namespace(netns: &str) {
    // Clean up any existing namespace first (from failed test runs)
    run_cmd_ignore_failure(&format!("{SUDO} ip netns del {netns}"));
    run_cmd(&format!("{SUDO} ip netns add {netns}"));
    // A fresh namespace has loopback down; bring it up so dora's management API
    // on 127.0.0.1 is reachable for the readiness probe in start_dhcp_server.
    // (DHCP itself uses the veth, so this doesn't affect the datapath.)
    run_cmd(&format!("{SUDO} ip netns exec {netns} ip link set lo up"));
}

fn remove_test_net_namespace(netns: &str) {
    run_cmd_ignore_failure(&format!("{SUDO} ip netns del {netns}"));
}

fn create_test_veth_nics(netns: &str, srv_ip: &str, veth_cli: &str, veth_srv: &str) {
    // Clean up any existing veth interfaces first (from failed test runs)
    run_cmd_ignore_failure(&format!("{SUDO} ip link del {veth_cli}"));
    run_cmd(&format!(
        "{SUDO} ip link add {veth_cli} type veth peer name {veth_srv}",
    ));
    run_cmd(&format!("{SUDO} ip link set {veth_cli} up"));
    run_cmd(&format!("{SUDO} ip link set {veth_srv} netns {netns}",));
    run_cmd(&format!(
        "{SUDO} ip netns exec {netns} ip link set {veth_srv} up",
    ));
    run_cmd(&format!(
        "{SUDO} ip netns exec {netns} ip addr add {srv_ip}/24 dev {veth_srv}",
    ));
    // TODO: remove this eventually
    run_cmd(&format!(
        "{SUDO} ip addr add 192.168.2.99/24 dev {veth_cli}"
    ));
}

fn remove_test_veth_nics(veth_cli: &str) {
    run_cmd_ignore_failure(&format!("{SUDO} ip link del {veth_cli}"));
}

fn start_dhcp_server(config: &str, netns: &str, db_url: &str) -> Child {
    let workspace_root = env::var("WORKSPACE_ROOT").unwrap_or_else(|_| "..".to_owned());
    let bin_path = env!("CARGO_BIN_EXE_dora");
    let config_path = format!("{workspace_root}/bin/tests/test_configs/{config}");
    let dora_debug = format!(
        "{bin_path} -d={db_url} --config-path={config_path} --threads=2 --dora-log=debug --v4-addr=0.0.0.0:9900",
    );
    let cmd = format!("{SUDO} ip netns exec {netns} {dora_debug}");

    let cmds: Vec<&str> = cmd.split(' ').collect();
    let mut child = Command::new(cmds[0])
        .args(&cmds[1..])
        // seems to mess up output formatting
        .stdin(Stdio::null())
        .spawn()
        .expect("Failed to start DHCP server");

    // Wait until dora is actually serving before the test sends. It connects to
    // Postgres and runs migrations at startup, so readiness is not instant and a
    // fixed sleep is fragile under CI load. Poll dora's health endpoint instead
    // (its management API binds 127.0.0.1:3333 inside the namespace); the v4
    // server is registered right after the API reports healthy. Bounded so a dora
    // that never comes up still falls through to the liveness check below.
    let health =
        format!("{SUDO} ip netns exec {netns} curl -sf --max-time 1 http://127.0.0.1:3333/health",);
    let health_cmd: Vec<&str> = health.split(' ').collect();
    for _ in 0..40 {
        if let Ok(Some(ret)) = child.try_wait() {
            panic!("Failed to start DHCP server {ret:?}");
        }
        let ready = Command::new(health_cmd[0])
            .args(&health_cmd[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ready {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(250));
    }
    if let Ok(Some(ret)) = child.try_wait() {
        panic!("Failed to start DHCP server {ret:?}");
    }
    child
}

fn stop_dhcp_server(daemon: &mut Child) {
    daemon.kill().expect("Failed to stop DHCP server")
}

fn run_cmd(cmd: &str) -> String {
    let cmds: Vec<&str> = cmd.split(' ').collect();
    let output = Command::new(cmds[0])
        .args(&cmds[1..])
        .output()
        .unwrap_or_else(|_| panic!("failed to execute command {cmd}"));
    if !output.status.success() {
        panic!("{}", String::from_utf8_lossy(&output.stderr));
    }

    String::from_utf8(output.stdout).expect("Failed to convert file command output to String")
}

fn run_cmd_ignore_failure(cmd: &str) -> String {
    let cmds: Vec<&str> = cmd.split(' ').collect();
    match Command::new(cmds[0]).args(&cmds[1..]).output() {
        Ok(o) => String::from_utf8(o.stdout).unwrap_or_default(),
        Err(e) => {
            eprintln!("Failed to execute command {cmd}: {e}");
            "".to_string()
        }
    }
}

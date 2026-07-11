use std::{
    env,
    process::{Child, Command, Stdio},
    thread,
};

use integration_tests::{bin_path, block_on};

#[derive(Debug)]
pub(crate) struct DhcpServerEnv {
    // one process per service now: the API (health endpoint) and the v4 server.
    // They share the per-test database; the schema is applied by `dora-migrate`
    // before either starts.
    daemons: Vec<Child>,
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
            daemons: start_services(config, netns, &db_url),
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
        for daemon in &mut self.daemons {
            stop_dhcp_server(daemon);
        }
        remove_test_veth_nics(&self.veth_cli);
        remove_test_net_namespace(&self.netns);
        // best-effort: the daemon is dead, so its connection is gone; WITH (FORCE)
        // in drop_test_database evicts any that lingers.
        if let Err(err) = block_on(ip_manager::postgres::drop_test_database(&self.db_name)) {
            eprintln!("failed to drop test database {}: {err:?}", self.db_name);
        }
    }
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

/// Run a command inside the test namespace (`sudo ip netns exec <netns> …`),
/// returning a `Command` ready to `.spawn()`/`.status()`. Arguments are passed
/// as discrete argv entries — never a space-split string — so a path containing
/// a space (e.g. `CARGO_MANIFEST_DIR` under a spaced checkout) stays a single
/// argument.
fn netns_command(netns: &str, program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(SUDO);
    cmd.args(["ip", "netns", "exec", netns, program]);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd
}

/// Run `dora-migrate` against the per-test database and wait for it to finish.
///
/// The services no longer migrate on startup, so the schema has to exist before
/// they connect. Runs inside the test namespace so it reaches Postgres over the
/// same veth the servers use.
fn run_migrate(netns: &str, db_url: &str) {
    let migrate = bin_path("dora-migrate");
    let status = netns_command(netns, &migrate, &["-d", db_url, "--dora-log", "info"])
        .status()
        .expect("failed to run dora-migrate");
    assert!(status.success(), "dora-migrate failed: {status:?}");
}

/// Bring up the services for a test: migrate the DB, then start `dora-api` (for
/// the health endpoint the readiness probe hits) and `dora-v4`, both inside the
/// namespace against the shared database. Returns the child processes so the
/// harness can kill them on drop.
fn start_services(config: &str, netns: &str, db_url: &str) -> Vec<Child> {
    // schema first — the services assume it exists.
    run_migrate(netns, db_url);

    let config_path = format!("{}/tests/test_configs/{config}", env!("CARGO_MANIFEST_DIR"));

    let spawn = |bin: &str, extra: &[&str]| -> Child {
        let mut args = vec![
            "-d",
            db_url,
            "--config-path",
            config_path.as_str(),
            "--threads",
            "2",
            "--dora-log",
            "debug",
        ];
        args.extend_from_slice(extra);
        netns_command(netns, bin, &args)
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to start {bin}: {e}"))
    };

    // the API provides the readiness signal; the v4 server serves the datapath.
    let mut children = vec![
        spawn(&bin_path("dora-api"), &[]),
        spawn(&bin_path("dora-v4"), &["--v4-addr", "0.0.0.0:9900"]),
    ];

    // Wait until the API is serving before the test sends. Both services connect
    // to Postgres at startup, so readiness is not instant and a fixed sleep is
    // fragile under CI load. Poll the API health endpoint (it binds
    // 127.0.0.1:3333 inside the namespace). Bounded so a service that never comes
    // up still falls through to the liveness check below.
    let assert_alive = |children: &mut [Child]| {
        for child in children {
            if let Ok(Some(ret)) = child.try_wait() {
                panic!("a dora service exited before becoming ready: {ret:?}");
            }
        }
    };
    for _ in 0..40 {
        assert_alive(&mut children);
        let ready = netns_command(
            netns,
            "curl",
            &["-sf", "--max-time", "1", "http://127.0.0.1:3333/health"],
        )
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
    assert_alive(&mut children);

    // The API's health only proves the API process is up. dora-v4 is a SEPARATE
    // process and may not have bound its UDP socket yet, so gate on the v4
    // listener too before the test starts sending (otherwise the first DISCOVER
    // races the bind). Falls through after the bound so the client's own retry
    // loop still covers a very late bind.
    wait_for_udp_port(netns, 9900, &mut children);

    children
}

/// Poll until a UDP socket is listening on `port` inside the namespace (via
/// `ss`), or a bounded number of tries elapse. Panics if a service dies first.
fn wait_for_udp_port(netns: &str, port: u16, children: &mut [Child]) {
    let needle = format!(":{port}");
    for _ in 0..40 {
        for child in children.iter_mut() {
            if let Ok(Some(ret)) = child.try_wait() {
                panic!("a dora service exited before becoming ready: {ret:?}");
            }
        }
        if let Ok(out) = netns_command(netns, "ss", &["-H", "-u", "-l", "-n"]).output()
            && String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.contains(&needle))
        {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(250));
    }
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

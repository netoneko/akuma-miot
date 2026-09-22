//! `kot` — see `lib.rs` for the parts, `docs/CLEANUP.md` item 2 for the
//! agreed interface.
//!
//! ```text
//! kot run --as <name> [--peers ...] [--llm URL | --glm]   node + agent loop
//! kot task open "<text>" | kot task list
//! kot artifact <id>
//! kot say "<body>" [--to <name>]
//! kot clear                                               root only
//! kot peers                                               roster + mesh
//! kot log [--task <id>] [--follow]
//! kot id --seed-file <path>                               make/show an identity
//! kot                                                     interactive REPL
//! ```
//!
//! Every flag has an env twin (`MIOT_*`). Flags are for interactive use,
//! env vars for a service unit or a herd conf — the same split the old
//! `miot` binary had.

use clap::{Args, Parser, Subcommand};
use kot::common::{self, expand_home, parse_account, Roster, DIM, OFF};
use kot::{agent, client, node};
use miot_keys::Identity;
use miot_runtime::RuntimeCall;

const DEV_ROSTER: &str = "root=1,mimi=2,tama=3,kuro=4,sora=5";

#[derive(Parser)]
#[command(name = "kot", version, about = "The litter's binary: a mesh node + agent loop (`kot run`), or a client of any node")]
struct Cli {
    /// A node to talk to. Falls back to each of --nodes in turn.
    #[arg(long, env = "MIOT_NODE", global = true)]
    node: Option<String>,
    /// More nodes to try, comma-separated: any swarm node will do.
    #[arg(long, env = "MIOT_NODES", global = true, value_delimiter = ',')]
    nodes: Vec<String>,
    /// `run`: this node's mesh name, and the cat it runs. Anything else:
    /// sign as this roster member instead of the operator's own identity.
    #[arg(long = "as", env = "MIOT_NAME", global = true)]
    as_: Option<String>,
    /// Sign with this seed (small int or 64 hex) instead.
    #[arg(long, env = "MIOT_SEED", global = true, hide_env_values = true)]
    seed: Option<String>,
    /// Sign with the seed in this file instead.
    #[arg(long, env = "MIOT_SEED_FILE", global = true)]
    seed_file: Option<String>,
    /// name=seed or name=pub:<hex>, comma-separated.
    #[arg(long, env = "MIOT_ROSTER", global = true, default_value = DEV_ROSTER)]
    roster: String,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// A mesh node, plus this cat's agent loop if given a model.
    Run(RunArgs),
    /// Open or list tasks.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// A closed parent's report, as markdown on stdout.
    Artifact { id: String },
    /// Say something to the litter, or one cat.
    Say {
        body: String,
        #[arg(long)]
        to: Option<String>,
    },
    /// Fail every open task: a new session, same chain. Root only.
    Clear,
    /// The litter roster, and the mesh as the connected node sees it.
    Peers,
    /// The event log.
    Log {
        #[arg(long)]
        task: Option<String>,
        #[arg(long, short)]
        follow: bool,
    },
    /// Create an identity at --seed-file if there isn't one, and print it.
    Id {
        /// Comment for the `.pub` line.
        #[arg(long, default_value = "kot")]
        comment: String,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    Open { text: String },
    List,
}

#[derive(Args)]
struct RunArgs {
    /// The other mesh members, as this node reaches them.
    #[arg(long, env = "MIOT_PEERS", value_delimiter = ',')]
    peers: Vec<String>,
    #[arg(long, env = "MIOT_PORT", default_value_t = 9944)]
    port: u16,
    #[arg(long, env = "MIOT_BIND", default_value = "0.0.0.0")]
    bind: String,
    /// The block log. Default: kot-<name>.db
    #[arg(long, env = "MIOT_DB")]
    db: Option<String>,
    /// A llama-server (or any OpenAI-compatible) base URL.
    #[arg(long, env = "MIOT_LLM", conflicts_with = "glm")]
    llm: Option<String>,
    /// GLM on z.ai, key read from --glm-token-file.
    #[arg(long, env = "MIOT_GLM")]
    glm: bool,
    #[arg(long, env = "MIOT_GLM_TOKEN_FILE", default_value = "~/.akuma/z.ai/token")]
    glm_token_file: String,
    /// Default: qwen3:4b, or glm-5.3 (z.ai coding plan) with --glm.
    #[arg(long, env = "MIOT_MODEL")]
    model: Option<String>,
    #[arg(long, env = "MIOT_PERSONA")]
    persona: Option<String>,
    /// Root: an authorized_keys line, 64-hex account, or dev seed. Genesis.
    #[arg(long, env = "MIOT_ROOT_PUBKEY", default_value = "1")]
    root: String,
    /// The litter leader (who plans) at genesis. Not the mesh primary.
    #[arg(long, env = "MIOT_LEADER", default_value = "2")]
    leader: String,
    /// Accounts that exist at genesis (hex or dev seeds). Genesis.
    #[arg(long, env = "MIOT_MEMBERS", value_delimiter = ',', default_value = "1,2,3,4,5")]
    members: Vec<String>,
    #[arg(long, env = "MIOT_BLOCK_MS", default_value_t = node::BLOCK_MS)]
    block_ms: u64,
    #[arg(long, env = "MIOT_SYNC_MS", default_value_t = 2000)]
    sync_ms: u64,
    #[arg(long, env = "MIOT_POLL_MS", default_value_t = 1000)]
    poll_ms: u64,
    #[arg(long, env = "MIOT_ELECTION_MIN_MS", default_value_t = 4000)]
    election_min_ms: u64,
    #[arg(long, env = "MIOT_ELECTION_MAX_MS", default_value_t = 8000)]
    election_max_ms: u64,
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("kot: {msg}");
    std::process::exit(2)
}

/// Who a command signs as: --seed, then --seed-file, then --as <roster
/// member>, then the operator's own persisted identity.
fn signer(cli: &Cli) -> Identity {
    if let Some(s) = &cli.seed {
        return Identity::from_seed(&common::parse_seed(s).unwrap_or_else(|e| die(e)));
    }
    if let Some(f) = &cli.seed_file {
        return common::read_seed_file(&expand_home(f)).unwrap_or_else(|e| die(e));
    }
    if let Some(name) = &cli.as_ {
        return common::roster_seed(&cli.roster, name)
            .unwrap_or_else(|| die(format!("--as {name}: no seed for {name} in the roster (a pub: entry can't sign)")));
    }
    common::load_or_create_identity(&common::root_identity_path(), "miot-root")
}

async fn connect(cli: &Cli) -> client::Client {
    let mut candidates: Vec<String> = cli.node.iter().cloned().collect();
    candidates.extend(cli.nodes.iter().filter(|n| !n.is_empty()).cloned());
    if candidates.is_empty() {
        candidates.push("http://127.0.0.1:9944".into());
    }
    let roster = Roster::parse(&cli.roster).unwrap_or_else(|e| die(e));
    client::Client::connect(candidates, signer(cli), roster).await.unwrap_or_else(|e| die(e))
}

async fn run(cli: &Cli, a: &RunArgs) {
    let name = cli.as_.clone().unwrap_or_else(|| die("run needs --as <name> (or MIOT_NAME)"));
    let account = |s: &str| parse_account(s).unwrap_or_else(|e| die(e));
    let cfg = node::NodeConfig {
        name: name.clone(),
        bind: a.bind.clone(),
        port: a.port,
        db: expand_home(a.db.as_deref().unwrap_or(&format!("kot-{name}.db"))),
        peers: a.peers.iter().map(|p| p.trim().trim_end_matches('/').to_string()).filter(|p| !p.is_empty()).collect(),
        root: account(&a.root),
        leader: account(&a.leader),
        members: a.members.iter().filter(|m| !m.trim().is_empty()).map(|m| account(m)).collect(),
        block_ms: a.block_ms,
        sync_ms: a.sync_ms,
        poll_ms: a.poll_ms,
        timing: miot_mesh::Timing { election_min_ms: a.election_min_ms, election_max_ms: a.election_max_ms },
    };
    let running = node::start(cfg).await.unwrap_or_else(|e| die(e));

    let llm = match (&a.llm, a.glm) {
        (Some(url), _) => Some(miot_llm::Llm::local(url, a.model.as_deref().unwrap_or("qwen3:4b"))),
        (None, true) => {
            let path = expand_home(&a.glm_token_file);
            let token = std::fs::read_to_string(&path).unwrap_or_else(|e| die(format!("--glm: {}: {e}", path.display())));
            Some(miot_llm::Llm::glm(&token, a.model.as_deref().unwrap_or("glm-5.3")))
        }
        (None, false) => None,
    };
    match llm {
        None => println!("[{name}] no --llm/--glm: node only, no agent loop"),
        Some(llm) => {
            let identity = if cli.seed.is_some() || cli.seed_file.is_some() {
                signer(cli)
            } else {
                common::roster_seed(&cli.roster, &name)
                    .unwrap_or_else(|| die(format!("the agent loop needs an identity: --seed-file, --seed, or a seed for {name} in the roster")))
            };
            let persona = a
                .persona
                .as_deref()
                .and_then(|p| std::fs::read_to_string(expand_home(p)).ok())
                .unwrap_or_else(|| format!("You are {name}, a cat in the Akuma Miot litter."));
            let cfg = agent::AgentConfig {
                name: name.clone(),
                identity,
                // Over HTTP even though it's this process (docs/CLI.md §5a).
                node: format!("http://127.0.0.1:{}", running.addr.port()),
                llm,
                persona,
                roster: Roster::parse(&cli.roster).unwrap_or_else(|e| die(e)),
            };
            tokio::spawn(agent::run(cfg));
        }
    }
    running.wait().await;
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match &cli.cmd {
        Some(Cmd::Run(a)) => run(&cli, a).await,
        Some(Cmd::Id { comment }) => {
            let path = expand_home(cli.seed_file.as_deref().unwrap_or_else(|| die("id needs --seed-file <path>")));
            let id = common::load_or_create_identity(&path, comment);
            println!("{}", miot_keys::to_hex(&id.account()));
            println!("{}", id.ssh_public_line(comment));
        }
        Some(Cmd::Task { cmd: TaskCmd::Open { text } }) => {
            let mut c = connect(&cli).await;
            let since = c.head_seq().await;
            if c.submit(RuntimeCall::Litter(pallet_litter::Call::open { text: text.clone() })).await {
                c.log(since, None, true, Some(8)).await;
            }
        }
        Some(Cmd::Task { cmd: TaskCmd::List }) => connect(&cli).await.print_tasks().await,
        Some(Cmd::Artifact { id }) => {
            if !connect(&cli).await.print_artifact(id).await {
                std::process::exit(1);
            }
        }
        Some(Cmd::Say { body, to }) => {
            let mut c = connect(&cli).await;
            let to = to.as_ref().map(|n| c.roster.account(n).unwrap_or_else(|| die(format!("no such cat: {n}"))));
            let since = c.head_seq().await;
            if c.submit(client::say_call(to, body)).await {
                c.log(since, None, true, Some(8)).await;
            }
        }
        Some(Cmd::Clear) => {
            let mut c = connect(&cli).await;
            c.submit(RuntimeCall::Litter(pallet_litter::Call::clear_all {})).await;
        }
        Some(Cmd::Peers) => connect(&cli).await.print_peers().await,
        Some(Cmd::Log { task, follow }) => connect(&cli).await.log(0, task.as_deref(), *follow, None).await,
        None => {
            println!("{}", include_str!("../../../assets/akuma_40.txt"));
            println!("  {DIM}akuma // distributed cat system{OFF}\n");
            client::repl(connect(&cli).await).await;
        }
    }
}

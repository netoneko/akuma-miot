//! `kot` — see `lib.rs` for the parts, `docs/CLEANUP.md` item 2 for the
//! agreed interface.
//!
//! ```text
//! kot run --as <name> [--peers ...] [--llm URL | --glm]   node + agent loop
//! kot chat [--llm URL | --glm]                            model only, no node
//! kot task open "<text>" | kot task list
//! kot artifact <id>
//! kot note <id> | kot notes                               standalone, no task
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
use kot::common::{self, expand_home, parse_account, Roster};
use kot::{agent, chat, client, node};
use miot_keys::Identity;
use miot_runtime::RuntimeCall;

// Deliberately *not* the deployed fleet's cat names (`mimi`/`tama`/`kuro`/
// `sora` — `overlays/deploy/deploy.py`'s `MIOT_ROSTER`). Those used to be
// mirrored here too, backed by small dev seeds instead of the fleet's real
// pubkeys — so a stale `MIOT_ROSTER` left in an operator's shell (from
// sourcing a real `kot.env`, or vice versa) silently resolved `@tama` to
// whichever account happened to be in scope, no error either way. Disjoint
// names turn that into a loud `no such cat` instead. `root` stays `root` in
// both: it's a role (`Authority::Root`), not a fleet persona — see
// `HANDOFF.md`'s "Next, in order" item 0 for the separate, not-yet-started
// proposal to rename that one too. `docs/LOCAL_SIM.md` has the walkthrough.
const DEV_ROSTER: &str = "root=1,simlead=2,sima=3,simb=4,simc=5";

#[derive(Parser)]
#[command(name = "kot", version = kot::version::VERSION, about = "The litter's binary: a mesh node + agent loop (`kot run`), or a client of any node")]
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
    /// name=seed or name=pub:<hex>, comma-separated. `run`: the genesis
    /// roster. A client doesn't need one — it reads the roster from the node
    /// (`/roster`); here it's only where `--as <name>` finds a dev seed.
    #[arg(long, env = "MIOT_ROSTER", global = true, default_value = DEV_ROSTER)]
    roster: String,
    /// The skin: bund (default), neon, or ink.
    #[arg(long, env = "KOT_THEME", global = true)]
    theme: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// A mesh node, plus this cat's agent loop if given a model.
    Run(RunArgs),
    /// Talk to a model directly in this process — no node, no chain.
    Chat(ChatArgs),
    /// Open or list tasks.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// A closed parent's report, as markdown on stdout.
    Artifact { id: String },
    /// A standalone note's markdown on stdout — no task behind it.
    Note { id: String },
    /// Every standalone note: id, title, author.
    Notes,
    /// Every artifact, task-closed and standalone merged: id, title,
    /// author. `Notes` only ever shows the standalone half of this.
    Artifacts,
    /// Publish a file's contents as a standalone artifact — anyone may
    /// (`pallet_litter::Call::publish_standalone_artifact` has no
    /// authority check), same as a cat's own `Artifact` tool call.
    Publish { file: String },
    /// Say something to the litter, or one cat.
    Say {
        body: String,
        #[arg(long)]
        to: Option<String>,
        /// Never write this one into the block log — it still wakes its
        /// recipient live, it just doesn't survive a replay or a rewind.
        #[arg(long)]
        off_record: bool,
    },
    /// Fail every open task: a new session, same chain. Root only.
    Clear,
    /// Ask the node to snapshot and shrink the block log now. Root only.
    Compact,
    /// The litter roster, and the mesh as the connected node sees it.
    Peers,
    /// What each cat is doing right now — its place in the agent loop, the
    /// tool calls in flight, how finished ones went, its last reasoning.
    Activity {
        /// Just this cat.
        name: Option<String>,
    },
    /// The event log. `--tree` renders just the conversation, threaded by
    /// reply parent (see `Client::log_tree`).
    Log {
        #[arg(long)]
        task: Option<String>,
        #[arg(long, short)]
        follow: bool,
        #[arg(long)]
        tree: bool,
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
    /// The litter's tasks — or, with --cat, that cat's own local list.
    List {
        /// A cat's own local task list (`LocalTask`), from its live activity.
        #[arg(long)]
        cat: Option<String>,
    },
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
    #[command(flatten)]
    llm: LlmArgs,
    #[arg(long, env = "MIOT_PERSONA")]
    persona: Option<String>,
    /// Extra system-prompt sections after the persona: comma-separated files
    /// or directories (every *.md, sorted). Shared facts — where the source
    /// is, what the projects are. docs/GIT_HOME.md §3.
    #[arg(long, env = "MIOT_CONTEXT", default_value = "")]
    context: String,
    /// Root: an authorized_keys line, 64-hex account, or dev seed. Genesis.
    #[arg(long, env = "MIOT_ROOT_PUBKEY", default_value = "1")]
    root: String,
    /// The litter leader (who plans) at genesis: a roster name, or anything
    /// --root takes. Not the mesh primary.
    #[arg(long, env = "MIOT_LEADER", default_value = "2")]
    leader: String,
    #[arg(long, env = "MIOT_BLOCK_MS", default_value_t = node::BLOCK_MS)]
    block_ms: u64,
    #[arg(long, env = "MIOT_SYNC_MS", default_value_t = 2000)]
    sync_ms: u64,
    #[arg(long, env = "MIOT_POLL_MS", default_value_t = 1000)]
    poll_ms: u64,
    /// A leader unheard for this long (randomized up to the max) triggers a
    /// pre-vote; a leader that can't see a majority for this long steps
    /// down. 4 s flapped live: a mac busy with a cargo build starved the
    /// Lima VM past four 1 s polls in a row, twice in a minute.
    #[arg(long, env = "MIOT_ELECTION_MIN_MS", default_value_t = 10_000)]
    election_min_ms: u64,
    #[arg(long, env = "MIOT_ELECTION_MAX_MS", default_value_t = 20_000)]
    election_max_ms: u64,
    /// Accounts outside the roster allowed to follow from this node —
    /// `name=pub:<64 hex>,...`. They read (status, block log, client GETs);
    /// they never vote, push blocks or count toward the quorum. Not genesis:
    /// set it on just the nodes a patron talks to.
    #[arg(long, env = "MIOT_PATRONS", default_value = "")]
    patrons: String,
    /// No agent loop: this cat's node runs, but no model is called, and every
    /// DM or @name tag is answered "*<name> is currently asleep*". Overrides
    /// --llm/--glm/--openrouter.
    #[arg(long, env = "MIOT_ASLEEP")]
    asleep: bool,
    /// Run as a patron: pull the chain, never campaign or vote, never
    /// produce. For a node whose key isn't in the roster; the members it
    /// polls must list it in their --patrons.
    #[arg(long, env = "MIOT_PATRON")]
    patron: bool,
    /// Give this cat the `Reboot` tool (compact, then reboot the host).
    /// Off by default — `docs/TOOLING.md` has why, and which cat (if any)
    /// this is actually set for.
    #[arg(long, env = "MIOT_REBOOT_TOOL")]
    reboot_tool: bool,
}

#[derive(Args)]
struct ChatArgs {
    #[command(flatten)]
    llm: LlmArgs,
    #[arg(long, env = "MIOT_PERSONA")]
    persona: Option<String>,
    /// As `run --context`.
    #[arg(long, env = "MIOT_CONTEXT", default_value = "")]
    context: String,
}

/// The persona plus every `--context` section; each unreadable path said once.
fn with_context(persona: String, spec: &str, name: &str) -> String {
    let (extra, problems) = common::load_context(spec);
    for p in &problems {
        eprintln!("[{name}] --context: {p} (skipped)");
    }
    persona + &extra
}

/// What a cat thinks with — shared by `run` and `chat`, so a service unit
/// and a one-off `chat` session build the same [`miot_llm::Llm`], each hosted
/// provider's key read from a token file rather than the environment.
#[derive(Args)]
struct LlmArgs {
    /// A llama-server (or any OpenAI-compatible, keyless) base URL.
    #[arg(long, env = "MIOT_LLM", conflicts_with_all = ["glm", "openrouter"])]
    llm: Option<String>,
    /// GLM on z.ai, key read from --glm-token-file.
    #[arg(long, env = "MIOT_GLM", conflicts_with = "openrouter")]
    glm: bool,
    #[arg(long, env = "MIOT_GLM_TOKEN_FILE", default_value = "~/.akuma/z.ai/token")]
    glm_token_file: String,
    /// Any OpenRouter model (--model is required: `moonshotai/kimi-k2`, …),
    /// key read from --openrouter-token-file.
    #[arg(long, env = "MIOT_OPENROUTER")]
    openrouter: bool,
    #[arg(long, env = "MIOT_OPENROUTER_TOKEN_FILE", default_value = "~/.akuma/openrouter/token")]
    openrouter_token_file: String,
    /// Default: qwen3:4b, or glm-5.3 (z.ai coding plan) with --glm. No
    /// default with --openrouter: an OpenRouter model is a spending choice.
    #[arg(long, env = "MIOT_MODEL")]
    model: Option<String>,
    /// How hard the model thinks per turn: none|minimal|low|medium|high, or
    /// `default` to leave it to the provider. Unset: `low` with --glm
    /// (`miot_llm::GLM_REASONING`), the provider's own default otherwise.
    #[arg(long, env = "MIOT_REASONING")]
    reasoning: Option<String>,
    /// The model's context window in tokens, for a hosted model that can't
    /// be asked (`--glm`, `--openrouter`) — what the loop's budget warnings
    /// and force-compaction measure against. A llama-server's is read from
    /// it; this overrides that too.
    #[arg(long, env = "MIOT_CONTEXT_WINDOW")]
    context_window: Option<u32>,
}

fn read_token(flag: &str, file: &str) -> String {
    let path = expand_home(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| die(format!("{flag}: {}: {e}", path.display())))
}

fn build_llm(a: &LlmArgs) -> Option<miot_llm::Llm> {
    let mut llm = build_llm_for(a)?;
    if let Some(n) = a.context_window {
        llm = llm.with_context_window(n);
    }
    Some(match &a.reasoning {
        Some(e) => llm.with_reasoning(e).unwrap_or_else(|e| die(format!("--reasoning: {e}"))),
        None => llm,
    })
}

fn build_llm_for(a: &LlmArgs) -> Option<miot_llm::Llm> {
    let model = a.model.as_deref();
    if let Some(url) = &a.llm {
        return Some(miot_llm::Llm::local(url, model.unwrap_or("qwen3:4b")));
    }
    if a.glm {
        return Some(miot_llm::Llm::glm(&read_token("--glm", &a.glm_token_file), model.unwrap_or("glm-5.3")));
    }
    if a.openrouter {
        let model = model.unwrap_or_else(|| die("--openrouter needs --model (an OpenRouter model id, e.g. qwen/qwen3-coder)"));
        return Some(miot_llm::Llm::openrouter(&read_token("--openrouter", &a.openrouter_token_file), model));
    }
    None
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
        candidates.push("https://127.0.0.1:9944".into());
    }
    let roster = Roster::parse(&cli.roster).unwrap_or_else(|e| die(e));
    client::Client::connect(candidates, signer(cli), roster).await.unwrap_or_else(|e| die(e))
}

async fn run(cli: &Cli, a: &RunArgs) {
    let name = cli.as_.clone().unwrap_or_else(|| die("run needs --as <name> (or MIOT_NAME)"));
    let account = |s: &str| parse_account(s).unwrap_or_else(|e| die(e));
    // Every node needs its own keypair now, not just one running an agent
    // loop: it signs the mesh-internal traffic (election, chain sync) this
    // node sends, election included — a node with no `--llm`/`--glm` used
    // to get no identity at all (docs/MESH_AUTH.md).
    let identity = if cli.seed.is_some() || cli.seed_file.is_some() {
        signer(cli)
    } else {
        common::roster_seed(&cli.roster, &name).unwrap_or_else(|| {
            die(format!("run needs a keypair to sign mesh traffic: --seed-file, --seed, or a seed for {name} in the roster"))
        })
    };
    // The genesis membership *is* the roster (`--roster`/`MIOT_ROSTER`),
    // committed to chain state by name — there is no separate members list
    // to drift from it any more.
    let roster = Roster::parse(&cli.roster).unwrap_or_else(|e| die(e));
    let root = account(&a.root);
    roster.check_genesis(&root).unwrap_or_else(|e| die(format!("--roster: {e}")));
    let leader = roster.account(&a.leader).unwrap_or_else(|| account(&a.leader));
    let patrons = Roster::parse(&a.patrons).unwrap_or_else(|e| die(format!("--patrons: {e}")));
    for (f, acct) in &patrons.0 {
        if roster.0.iter().any(|(_, m)| m == acct) || *acct == root || *acct == leader {
            die(format!("--patrons: {f} is already a genesis member; a member doesn't need to be a patron"));
        }
    }
    let cfg = node::NodeConfig {
        name: name.clone(),
        identity,
        bind: a.bind.clone(),
        port: a.port,
        db: expand_home(a.db.as_deref().unwrap_or(&format!("kot-{name}.db"))),
        peers: a.peers.iter().map(|p| p.trim().trim_end_matches('/').to_string()).filter(|p| !p.is_empty()).collect(),
        root,
        leader,
        roster: roster.0.clone(),
        block_ms: a.block_ms,
        sync_ms: a.sync_ms,
        poll_ms: a.poll_ms,
        timing: miot_mesh::Timing { election_min_ms: a.election_min_ms, election_max_ms: a.election_max_ms },
        patrons: patrons.0,
        learner: a.patron,
    };
    let running = node::start(cfg).await.unwrap_or_else(|e| die(e));

    let node_url = format!("https://127.0.0.1:{}", running.addr.port());
    if a.asleep {
        let roster = Roster::parse(&cli.roster).unwrap_or_else(|e| die(e));
        tokio::spawn(agent::run_asleep(name.clone(), identity, node_url, roster));
        running.wait().await;
        return;
    }
    let llm = build_llm(&a.llm);
    match llm {
        None => println!("[{name}] no --llm/--glm/--openrouter: node only, no agent loop"),
        Some(llm) => {
            let persona = a
                .persona
                .as_deref()
                .and_then(|p| std::fs::read_to_string(expand_home(p)).ok())
                .unwrap_or_else(|| format!("You are {name}, a cat in the Akuma Miot litter."));
            let persona = with_context(persona, &a.context, &name);
            let cfg = agent::AgentConfig {
                name: name.clone(),
                identity,
                // Over the network stack even though it's this process
                // (docs/CLI.md §5a) — mTLS included, same as any other caller.
                node: format!("https://127.0.0.1:{}", running.addr.port()),
                llm,
                persona,
                roster: Roster::parse(&cli.roster).unwrap_or_else(|e| die(e)),
                reboot_tool: a.reboot_tool,
            };
            tokio::spawn(agent::run(cfg));
        }
    }
    running.wait().await;
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // `ui::theme()` reads KOT_THEME once, on first use — set it before
    // anything renders, so the flag and the env var are the same thing.
    if let Some(t) = &cli.theme {
        // SAFETY: single-threaded here — the tokio runtime's workers exist,
        // but nothing has been spawned onto them yet.
        unsafe { std::env::set_var("KOT_THEME", t) };
    }
    match &cli.cmd {
        Some(Cmd::Run(a)) => run(&cli, a).await,
        Some(Cmd::Chat(a)) => {
            let llm = build_llm(&a.llm).unwrap_or_else(|| die("chat needs --llm <url>, --glm or --openrouter"));
            let name = cli.as_.clone().unwrap_or_else(|| "cat".to_string());
            let persona = a
                .persona
                .as_deref()
                .and_then(|p| std::fs::read_to_string(expand_home(p)).ok())
                .unwrap_or_else(|| format!("You are {name}, a cat, talking directly to your operator — no task, no chain, just conversation."));
            let persona = with_context(persona, &a.context, &name);
            chat::run(chat::ChatConfig { name, llm, persona }).await;
        }
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
        Some(Cmd::Task { cmd: TaskCmd::List { cat: None } }) => connect(&cli).await.print_tasks().await,
        Some(Cmd::Task { cmd: TaskCmd::List { cat: Some(cat) } }) => {
            let mut c = connect(&cli).await;
            println!("{}", c.local_tasks_text(cat).await);
        }
        Some(Cmd::Artifact { id }) => {
            if !connect(&cli).await.print_artifact(id).await {
                std::process::exit(1);
            }
        }
        Some(Cmd::Note { id }) => {
            if !connect(&cli).await.print_note(id).await {
                std::process::exit(1);
            }
        }
        Some(Cmd::Notes) => {
            connect(&cli).await.print_notes().await;
        }
        Some(Cmd::Artifacts) => {
            connect(&cli).await.print_artifacts().await;
        }
        Some(Cmd::Publish { file }) => {
            let text = std::fs::read_to_string(expand_home(file)).unwrap_or_else(|e| die(format!("{file}: {e}")));
            let mut c = connect(&cli).await;
            let since = c.head_seq().await;
            if c.submit(RuntimeCall::Litter(pallet_litter::Call::publish_standalone_artifact { text })).await {
                c.log(since, None, true, Some(8)).await;
            }
        }
        Some(Cmd::Say { body, to, off_record }) => {
            let mut c = connect(&cli).await;
            let to = to.as_ref().map(|n| c.roster.account(n).unwrap_or_else(|| die(format!("no such cat: {n}"))));
            let since = c.head_seq().await;
            if c.submit(client::say_call(to, body, *off_record)).await {
                c.log(since, None, true, Some(8)).await;
            }
        }
        Some(Cmd::Clear) => {
            let mut c = connect(&cli).await;
            c.submit(RuntimeCall::Litter(pallet_litter::Call::clear_all {})).await;
        }
        Some(Cmd::Compact) => {
            let mut c = connect(&cli).await;
            c.submit(RuntimeCall::Litter(pallet_litter::Call::request_compaction {})).await;
        }
        Some(Cmd::Peers) => connect(&cli).await.print_peers().await,
        Some(Cmd::Activity { name }) => {
            let mut c = connect(&cli).await;
            println!("{}", c.activity_text(name.as_deref()).await);
        }
        Some(Cmd::Log { task, follow, tree }) => {
            let mut c = connect(&cli).await;
            if *tree {
                c.log_tree().await;
            } else {
                c.log(0, task.as_deref(), *follow, None).await;
            }
        }
        // The banner is `repl()`'s own (`ui::banner`) — printing the bare
        // cat here too drew the logo twice.
        None => client::repl(connect(&cli).await).await,
    }
}

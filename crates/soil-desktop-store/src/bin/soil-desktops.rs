//! `soil-desktops`: manage Teams, Desktops and share tickets in one records book.
//!
//! This is the host-side surface the Desktops Spore will call. It exists first
//! because the store had to be provable before a UI could be honest about it:
//! every verb here is the same function the Spore will reach through a host
//! capability, and every refusal is the model's, not a second implementation.
//!
//! The actor is explicit. There is no "current user" fallback: a command that
//! changes membership or mints a ticket must say who is asking, so a script
//! cannot accidentally inherit a shell identity.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use soil_desktop::{Item, Owner, PrincipalId, Role, Scope, TicketRequest};
use soil_desktop_store::{DesktopStore, Error};

#[derive(Parser)]
#[command(
    version,
    about = "Manage Teams, Desktops and share tickets over soil:records"
)]
struct Cli {
    /// Records book that holds the directory. The host picks it; no Spore does.
    #[arg(long, default_value = "desktops")]
    book: String,
    /// Records root.
    #[arg(long, default_value = "~/.local/share/fungos/desktop-records")]
    root: String,
    /// The principal asking.
    #[arg(long = "as")]
    actor: String,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Team {
        #[command(subcommand)]
        command: TeamCmd,
    },
    Desktop {
        #[command(subcommand)]
        command: DesktopCmd,
    },
    /// Mint a ticket for one item to one principal.
    Share {
        desktop: String,
        /// `file:<path>` or `record:<collection>#<id>`.
        item: String,
        principal: String,
        #[arg(long, value_enum, default_value_t = RoleArg::Reader)]
        role: RoleArg,
        /// Seconds until the ticket expires. Omit for no expiry.
        #[arg(long)]
        ttl_secs: Option<i64>,
        #[arg(long, default_value_t = 1)]
        uses: u32,
    },
    /// Redeem one use of a ticket.
    Redeem { desktop: String, ticket: String },
    /// Show the strongest role the actor holds for one item.
    Access { desktop: String, item: String },
    /// Show the records book, Geode address and VFS root for a Desktop.
    Key { desktop: String },
}

#[derive(Subcommand)]
enum TeamCmd {
    List,
    Create {
        id: String,
        name: String,
    },
    Show {
        id: String,
    },
    /// Add, change or remove a member. Owners only.
    Member {
        team: String,
        principal: String,
        #[arg(long, value_enum)]
        role: Option<RoleArg>,
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Subcommand)]
enum DesktopCmd {
    List,
    Show {
        id: String,
    },
    Create {
        id: String,
        name: String,
        #[arg(long, value_enum, default_value_t = ScopeArg::Personal)]
        scope: ScopeArg,
        /// Use the owner team. Requires `--scope team`.
        #[arg(long)]
        team: Option<String>,
    },
    /// Add, change or remove a member. Owners only.
    Member {
        desktop: String,
        principal: String,
        #[arg(long, value_enum)]
        role: Option<RoleArg>,
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RoleArg {
    Reader,
    Writer,
    Owner,
}

impl From<RoleArg> for Role {
    fn from(role: RoleArg) -> Self {
        match role {
            RoleArg::Reader => Role::Reader,
            RoleArg::Writer => Role::Writer,
            RoleArg::Owner => Role::Owner,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ScopeArg {
    Personal,
    Team,
    Org,
}

impl From<ScopeArg> for Scope {
    fn from(scope: ScopeArg) -> Self {
        match scope {
            ScopeArg::Personal => Scope::Personal,
            ScopeArg::Team => Scope::Team,
            ScopeArg::Org => Scope::Org,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("soil-desktops: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    let store = DesktopStore::open(expand_home(&cli.root), &cli.book)?;
    let actor = PrincipalId::new(cli.actor);
    let now = now_ms();

    match cli.command {
        Cmd::Team { command } => match command {
            TeamCmd::List => print(&store.teams()?)?,
            TeamCmd::Show { id } => print(&store.team(&id)?.ok_or(Error::NotFound)?)?,
            TeamCmd::Create { id, name } => print(&store.create_team(&actor, &id, &name)?)?,
            TeamCmd::Member {
                team,
                principal,
                role,
                remove,
            } => {
                let role = if remove {
                    None
                } else {
                    Some(role.unwrap_or(RoleArg::Reader).into())
                };
                print(&store.set_team_member(
                    &actor,
                    &team,
                    &PrincipalId::new(principal),
                    role,
                    now,
                )?)?;
            }
        },
        Cmd::Desktop { command } => match command {
            DesktopCmd::List => print(&store.desktops()?)?,
            DesktopCmd::Show { id } => print(&store.desktop(&id)?.ok_or(Error::NotFound)?)?,
            DesktopCmd::Create {
                id,
                name,
                scope,
                team,
            } => {
                let scope = Scope::from(scope);
                let owner = match (scope, team) {
                    (Scope::Team, Some(team)) => Owner::team(team),
                    (Scope::Team, None) => {
                        return Err(soil_desktop::Invalid::new(
                            "desktop.owner",
                            "--scope team needs --team",
                        )
                        .into());
                    }
                    (_, Some(_)) => {
                        return Err(soil_desktop::Invalid::new(
                            "desktop.owner",
                            "--team is only valid with --scope team",
                        )
                        .into());
                    }
                    (_, None) => Owner::principal(actor.clone()),
                };
                print(&store.create_desktop(&actor, &id, &name, scope, owner, now)?)?;
            }
            DesktopCmd::Member {
                desktop,
                principal,
                role,
                remove,
            } => {
                let role = if remove {
                    None
                } else {
                    Some(role.unwrap_or(RoleArg::Reader).into())
                };
                print(&store.set_desktop_member(
                    &actor,
                    &desktop,
                    &PrincipalId::new(principal),
                    role,
                    now,
                )?)?;
            }
        },
        Cmd::Share {
            desktop,
            item,
            principal,
            role,
            ttl_secs,
            uses,
        } => {
            let item = Item::parse(&item)?;
            let ticket = store.issue_ticket(
                &actor,
                &desktop,
                TicketRequest {
                    id: String::new(),
                    item,
                    grantee: PrincipalId::new(principal),
                    role: role.into(),
                    expires_at_ms: ttl_secs.map(|seconds| now + seconds * 1000),
                    max_uses: uses,
                },
                now,
            )?;
            print(&ticket)?;
        }
        Cmd::Redeem { desktop, ticket } => {
            let grant = store.redeem_ticket(&actor, &desktop, &ticket, now)?;
            print(&json!({
                "ticket": grant.ticket,
                "item": grant.item.token(),
                "role": grant.role,
            }))?;
        }
        Cmd::Access { desktop, item } => {
            let item = Item::parse(&item)?;
            let role = store.access(&actor, &desktop, &item, now)?;
            print(&json!({ "desktop": desktop, "item": item.token(), "role": role }))?;
        }
        Cmd::Key { desktop } => {
            let key = store.key(&desktop)?;
            print(&json!({
                "desktop": key.id(),
                "vfs": key.vfs(),
                "records": key.records_book("records")?,
                "geode": key.geode("records")?,
            }))?;
        }
    }
    Ok(())
}

fn print<T: serde::Serialize>(value: &T) -> Result<(), Error> {
    let text = serde_json::to_string_pretty(value).map_err(Error::from)?;
    println!("{text}");
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

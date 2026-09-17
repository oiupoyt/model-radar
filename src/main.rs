mod catalog;
mod store;

use catalog::Model;
use poise::serenity_prelude as serenity;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use store::{State, Store, Subscription};
use tokio::sync::Mutex;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

struct Data {
    store: Arc<Mutex<Store>>,
    support_url: Option<String>,
}

fn plain(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .map(|c| {
            if matches!(
                c,
                '`' | '*' | '_' | '~' | '[' | ']' | '<' | '>' | '@' | '|' | '\\'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn no_mentions() -> serenity::CreateAllowedMentions {
    serenity::CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .replied_user(false)
}

fn embed(title: &str) -> serenity::CreateEmbed {
    serenity::CreateEmbed::new()
        .title(title)
        .color(0x8367ef)
        .footer(serenity::CreateEmbedFooter::new(
            "Model Radar • Catalog prices, not unlimited access • Limits may apply",
        ))
}

async fn reply(ctx: Context<'_>, message: serenity::CreateEmbed) -> Result<(), Error> {
    ctx.send(
        poise::CreateReply::default()
            .embed(message)
            .allowed_mentions(no_mentions())
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

#[poise::command(slash_command)]
async fn models(
    ctx: Context<'_>,
    #[description = "Filter model ID or name"] search: Option<String>,
    #[description = "Page number"]
    #[min = 1]
    page: Option<u32>,
) -> Result<(), Error> {
    let guard = ctx.data().store.lock().await;
    let query = search.unwrap_or_default().to_lowercase();
    let matches: Vec<_> = guard
        .state
        .catalog
        .values()
        .filter(|m| {
            m.is_free()
                && (m.id.to_lowercase().contains(&query) || m.name.to_lowercase().contains(&query))
        })
        .collect();
    let pages = matches.len().div_ceil(10).max(1);
    let page = (page.unwrap_or(1) as usize).clamp(1, pages);
    let text = matches
        .iter()
        .skip((page - 1) * 10)
        .take(10)
        .map(|m| {
            format!(
                "**{}**\n`{}` · {} tokens",
                plain(&m.name, 90),
                plain(&m.id, 120),
                m.context_length
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let result = embed(&format!(
        "Free models · {} found · {page}/{pages}",
        matches.len()
    ))
    .description(if text.is_empty() {
        "No matching free models.".to_owned()
    } else {
        text
    });
    drop(guard);
    reply(ctx, result).await
}

#[poise::command(slash_command)]
async fn model(
    ctx: Context<'_>,
    #[description = "Exact model ID from /models"] id: String,
) -> Result<(), Error> {
    let found = ctx
        .data()
        .store
        .lock()
        .await
        .state
        .catalog
        .get(&id)
        .cloned();
    let Some(m) = found else {
        return reply(
            ctx,
            embed("Model not found").description("Use /models to find an exact model ID."),
        )
        .await;
    };
    let pricing = m
        .pricing
        .iter()
        .take(12)
        .map(|(key, value)| format!("{}: {}", plain(key, 35), plain(value, 35)))
        .collect::<Vec<_>>()
        .join("\n");
    reply(
        ctx,
        embed(&plain(&m.name, 200))
            .description(plain(&m.description, 2000))
            .field("Model ID", plain(&m.id, 200), false)
            .field("Context", format!("{} tokens", m.context_length), true)
            .field(
                "Free catalog pricing",
                if m.is_free() { "Yes" } else { "No / unknown" },
                true,
            )
            .field(
                "Raw USD prices (prompt/completion per token)",
                if pricing.is_empty() {
                    "Unavailable".into()
                } else {
                    pricing
                },
                false,
            ),
    )
    .await
}

#[poise::command(
    slash_command,
    guild_only,
    required_permissions = "MANAGE_GUILD",
    default_member_permissions = "MANAGE_GUILD"
)]
async fn watch(
    ctx: Context<'_>,
    #[description = "Optional opt-in, mentionable notification role"] role: Option<serenity::Role>,
    #[description = "Only free releases and paid-to-free changes (default true)"] free_only: Option<
        bool,
    >,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = ctx.guild_id().ok_or("Server required")?;
    if let Some(role) = &role
        && (role.guild_id != guild
            || role.id.get() == guild.get()
            || !role.mentionable
            || role.managed)
    {
        return reply(
            ctx,
            embed("Choose an opt-in role").description(
                "Use a mentionable, unmanaged role from this server; @everyone is not allowed.",
            ),
        )
        .await;
    }
    let channel = ctx
        .channel_id()
        .to_channel(ctx.serenity_context())
        .await?
        .guild()
        .ok_or("Server channel required")?;
    if channel.kind != serenity::ChannelType::Text || channel.guild_id != guild {
        return reply(
            ctx,
            embed("Text channel required")
                .description("Run /watch in a regular server text channel."),
        )
        .await;
    }
    let bot_id = ctx.serenity_context().cache.current_user().id;
    let member = guild.member(ctx.serenity_context(), bot_id).await?;
    let permissions = ctx
        .guild()
        .ok_or("Server cache unavailable")?
        .user_permissions_in(&channel, &member);
    if !permissions.contains(
        serenity::Permissions::VIEW_CHANNEL
            | serenity::Permissions::SEND_MESSAGES
            | serenity::Permissions::EMBED_LINKS,
    ) {
        return reply(
            ctx,
            embed("Missing bot permissions")
                .description("Allow View Channel, Send Messages, and Embed Links here first."),
        )
        .await;
    }
    let mut guard = ctx.data().store.lock().await;
    let mut next = guard.state.clone();
    next.subscriptions.insert(
        guild.get(),
        Subscription {
            channel_id: channel.id.get(),
            role_id: role.map(|r| r.id.get()),
            free_only: free_only.unwrap_or(true),
            pending: Vec::new(),
        },
    );
    guard.commit(next).await?;
    drop(guard);
    reply(ctx, embed("Alerts enabled here").description("Checking every 10 minutes. Only future detections are announced. Members can opt into your notification role. Run /watch again to change settings; /unwatch stops alerts. One channel per server.")).await
}

#[poise::command(
    slash_command,
    guild_only,
    required_permissions = "MANAGE_GUILD",
    default_member_permissions = "MANAGE_GUILD"
)]
async fn unwatch(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = ctx.guild_id().ok_or("Server required")?;
    let mut guard = ctx.data().store.lock().await;
    let mut next = guard.state.clone();
    next.subscriptions.remove(&guild.get());
    guard.commit(next).await?;
    drop(guard);
    reply(
        ctx,
        embed("Alerts disabled")
            .description("This server's subscription and pending alerts have been removed."),
    )
    .await
}

#[poise::command(slash_command)]
async fn stats(ctx: Context<'_>) -> Result<(), Error> {
    let guard = ctx.data().store.lock().await;
    let last = guard
        .state
        .last_success
        .map(|t| format!("<t:{t}:R>"))
        .unwrap_or_else(|| "Never".into());
    let mut result = embed("Radar status")
        .field("Catalog", guard.state.catalog.len().to_string(), true)
        .field(
            "Free",
            guard
                .state
                .catalog
                .values()
                .filter(|m| m.is_free())
                .count()
                .to_string(),
            true,
        )
        .field("Last successful fetch", last, false);
    if let Some(sub) = ctx
        .guild_id()
        .and_then(|id| guard.state.subscriptions.get(&id.get()))
    {
        result = result.field(
            "This server",
            format!(
                "Channel: <#{}>\nScope: {}\nQueued alerts: {}",
                sub.channel_id,
                if sub.free_only {
                    "Free models"
                } else {
                    "All new models + becoming free"
                },
                sub.pending.len()
            ),
            false,
        );
    }
    drop(guard);
    reply(ctx, result).await
}

#[poise::command(slash_command)]
async fn about(ctx: Context<'_>) -> Result<(), Error> {
    let mut result = embed("Model Radar")
        .description("OpenRouter's free model catalog, with opt-in release alerts.\n\n/models — browse free models\n/model — details and raw pricing\n/watch — configure alerts here (Manage Server)\n/unwatch — remove subscription\n/stats — freshness and settings\n\nFree to use. No OpenRouter key needed. New means newly observed in the catalog, not a verified launch date. Pricing and availability can change; provider limits apply. Server/channel/role IDs are stored for alerts; /unwatch deletes the subscription.");
    if let Some(url) = &ctx.data().support_url {
        result = result.field(
            "Optional support",
            format!("[Support the operator]({url}) — donations keep the bot free."),
            false,
        );
    }
    reply(ctx, result).await
}

fn updated(mut state: State, current: BTreeMap<String, Model>) -> State {
    if state.initialized {
        let events = catalog::changes(&state.catalog, &current);
        for sub in state.subscriptions.values_mut() {
            sub.pending.extend(
                events
                    .iter()
                    .filter(|event| !sub.free_only || event.model.is_free())
                    .cloned(),
            );
        }
    }
    state.catalog = current;
    state.initialized = true;
    state.last_success = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    state
}

async fn refresh(store: &Mutex<Store>, client: &reqwest::Client) -> Result<(), Error> {
    let current = catalog::fetch(client).await?;
    let mut guard = store.lock().await;
    let next = updated(guard.state.clone(), current);
    guard.commit(next).await
}

async fn deliver(store: &Mutex<Store>, http: &serenity::Http) -> Result<(), Error> {
    let guilds: Vec<_> = store
        .lock()
        .await
        .state
        .subscriptions
        .keys()
        .copied()
        .collect();
    for guild in guilds {
        let mut guard = store.lock().await;
        let Some(sub) = guard.state.subscriptions.get(&guild) else {
            continue;
        };
        if sub.pending.is_empty() {
            continue;
        }
        let count = sub.pending.len().min(8);
        let text = sub
            .pending
            .iter()
            .take(count)
            .map(|event| {
                format!(
                    "**{} · {}**\n`{}` · {} tokens · {}",
                    if event.kind == "paid_to_free" {
                        "Now free"
                    } else {
                        "New in catalog"
                    },
                    plain(&event.model.name, 80),
                    plain(&event.model.id, 120),
                    event.model.context_length,
                    if event.model.is_free() {
                        "Free pricing"
                    } else {
                        "Paid / unknown pricing"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut mentions = no_mentions();
        let mut message = serenity::CreateMessage::new()
            .embed(embed("Model Radar · catalog update").description(text));
        if let Some(role) = sub.role_id {
            mentions = mentions.roles([serenity::RoleId::new(role)]);
            message = message.content(format!("<@&{role}>"));
        }
        let sent = serenity::ChannelId::new(sub.channel_id)
            .send_message(http, message.allowed_mentions(mentions))
            .await;
        if sent.is_err() {
            tracing::warn!(guild, "Alert delivery failed; retained for next poll");
            continue;
        }
        let mut next = guard.state.clone();
        if let Some(sub) = next.subscriptions.get_mut(&guild) {
            sub.pending.drain(..count);
        }
        guard.commit(next).await?;
    }
    Ok(())
}

fn support_url() -> Result<Option<String>, Error> {
    let Ok(raw) = std::env::var("SUPPORT_URL") else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(&raw)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || raw
            .chars()
            .any(|c| c.is_control() || matches!(c, '(' | ')' | '<' | '>'))
        || raw.len() > 500
    {
        return Err("SUPPORT_URL must be a plain HTTPS URL without credentials".into());
    }
    Ok(Some(url.to_string()))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "model_radar=info".into()),
        )
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Model Radar\n\nRun: DISCORD_TOKEN from your environment; cargo run --locked\nCheck public catalog without Discord: cargo run --locked -- --check\nOptional: SUPPORT_URL (HTTPS donation link), STATE_PATH (default state.json), DISCORD_GUILD_ID (instant test-server registration).\n\nCreate a bot in Discord Developer Portal. Invite using bot + applications.commands scopes with View Channel, Send Messages, Embed Links. No privileged intents or Administrator permission needed. Keep the token secret; never paste it in chat.\nRun /watch in a server text channel with Manage Server permission. Optionally choose a mentionable opt-in role. Default alerts cover free models; free_only:false includes paid releases.\nOne process per state file. Keep the machine running for 24/7 alerts. State uses atomic writes; failed deliveries retry next poll; a crash after sending may duplicate an alert. First fetch is a silent baseline. Catalog reappearances count as new detections.\nChecks: cargo fmt --check; cargo test; cargo check; cargo clippy --all-targets -- -D warnings"
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg != "--check") {
        return Err("Unknown argument; use --help".into());
    }
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("model-radar/0.1")
        .build()?;
    if args.iter().any(|arg| arg == "--check") {
        let catalog = catalog::fetch(&http_client).await?;
        println!(
            "Catalog OK: {} models, {} free",
            catalog.len(),
            catalog.values().filter(|m| m.is_free()).count()
        );
        return Ok(());
    }
    let token =
        std::env::var("DISCORD_TOKEN").map_err(|_| "Set DISCORD_TOKEN locally; see --help")?;
    let support_url = support_url()?;
    let guild_id = std::env::var("DISCORD_GUILD_ID")
        .ok()
        .map(|id| id.parse::<u64>())
        .transpose()?;
    if guild_id == Some(0) {
        return Err("DISCORD_GUILD_ID must be nonzero".into());
    }
    let path = PathBuf::from(std::env::var("STATE_PATH").unwrap_or_else(|_| "state.json".into()));
    let store = Arc::new(Mutex::new(Store::load(path).await?));
    if let Err(error) = refresh(&store, &http_client).await {
        if !store.lock().await.state.initialized {
            return Err(error);
        }
        tracing::warn!("Initial catalog fetch failed; using saved state and retrying");
    }
    let mut commands = vec![models(), model(), watch(), unwatch(), stats(), about()];
    let descriptions = [
        "Browse OpenRouter's free models",
        "Inspect a model's details and pricing",
        "Enable release alerts in this channel",
        "Remove this server's alerts",
        "Show catalog freshness and alert settings",
        "About Model Radar and optional support",
    ];
    for (command, description) in commands.iter_mut().zip(descriptions) {
        command.description = Some(description.into());
    }
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands,
            on_error: |error| Box::pin(async move {
                if let Some(ctx) = error.ctx() {
                    let _ = reply(ctx, embed("Request failed").description("Check your permissions and arguments, or retry shortly. The operator can check bot logs.")).await;
                }
                tracing::warn!("Discord command/framework error");
            }),
            ..Default::default()
        })
        .setup(move |ctx, _, framework| Box::pin(async move {
            if let Some(id) = guild_id {
                poise::builtins::register_in_guild(ctx, &framework.options().commands, serenity::GuildId::new(id)).await?;
            } else {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
            }
            let poll_store = Arc::clone(&store);
            let http = Arc::clone(&ctx.http);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(600));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    if refresh(&poll_store, &http_client).await.is_err() { tracing::warn!("Catalog refresh failed; preserving previous snapshot"); }
                    if deliver(&poll_store, &http).await.is_err() { tracing::warn!("Saving delivery progress failed; alerts may retry"); }
                }
            });
            tracing::info!("Model Radar connected; catalog polling every 10 minutes");
            Ok(Data { store, support_url })
        }))
        .build();
    let mut client = serenity::ClientBuilder::new(token, serenity::GatewayIntents::GUILDS)
        .framework(framework)
        .await?;
    tokio::select! {
        result = client.start() => result?,
        result = tokio::signal::ctrl_c() => { result?; client.shard_manager.shutdown_all().await; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(free: bool) -> BTreeMap<String, Model> {
        let m = Model {
            id: "test/model".into(),
            name: "Test".into(),
            description: String::new(),
            context_length: 10,
            pricing: BTreeMap::from([
                ("prompt".into(), if free { "0" } else { "1" }.into()),
                ("completion".into(), "0".into()),
            ]),
        };
        BTreeMap::from([(m.id.clone(), m)])
    }

    #[test]
    fn baseline_is_silent_and_alerts_are_filtered() {
        let mut state = State::default();
        for (id, free_only) in [(1, true), (2, false)] {
            state.subscriptions.insert(
                id,
                Subscription {
                    channel_id: id,
                    role_id: None,
                    free_only,
                    pending: vec![],
                },
            );
        }
        let baseline = updated(state, catalog(false));
        assert!(
            baseline
                .subscriptions
                .values()
                .all(|s| s.pending.is_empty())
        );
        let free = updated(baseline, catalog(true));
        assert!(free.subscriptions.values().all(|s| s.pending.len() == 1));
        let unchanged = updated(free, catalog(true));
        assert!(
            unchanged
                .subscriptions
                .values()
                .all(|s| s.pending.len() == 1)
        );
        let mut next = catalog(true);
        let mut paid = catalog(false).remove("test/model").unwrap();
        paid.id = "paid/new".into();
        next.insert(paid.id.clone(), paid);
        let result = updated(unchanged, next);
        assert_eq!(result.subscriptions[&1].pending.len(), 1);
        assert_eq!(result.subscriptions[&2].pending.len(), 2);
    }

    #[test]
    fn external_text_is_bounded_and_sanitized() {
        assert_eq!(plain("@everyone **test**\n", 100), " everyone   test  ");
        assert_eq!(plain("ééé", 2), "éé");
    }
}

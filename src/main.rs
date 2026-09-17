mod catalog;
mod presentation;
mod store;

use catalog::Model;
use poise::serenity_prelude as serenity;
use presentation::{Browse, embed, no_mentions, text};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    poll_seconds: u64,
}

fn response(message: serenity::CreateEmbed, public: bool) -> poise::CreateReply {
    poise::CreateReply::default()
        .embed(message)
        .allowed_mentions(no_mentions())
        .ephemeral(!public)
}

async fn visible_reply(
    ctx: Context<'_>,
    message: serenity::CreateEmbed,
    public: bool,
) -> Result<(), Error> {
    ctx.send(response(message, public)).await?;
    Ok(())
}

async fn reply(ctx: Context<'_>, message: serenity::CreateEmbed) -> Result<(), Error> {
    visible_reply(ctx, message, false).await
}

async fn browse(ctx: Context<'_>, options: Browse, public: bool) -> Result<(), Error> {
    if public {
        ctx.defer().await?;
    } else {
        ctx.defer_ephemeral().await?;
    }
    let result = {
        let guard = ctx.data().store.lock().await;
        presentation::snapshot(&guard.state, &options, ctx.data().poll_seconds)
    };
    visible_reply(ctx, result, public).await
}

#[poise::command(slash_command)]
async fn models(
    ctx: Context<'_>,
    #[description = "Filter model ID or name"] search: Option<String>,
    #[description = "Page number"]
    #[min = 1]
    page: Option<u32>,
) -> Result<(), Error> {
    browse(
        ctx,
        Browse {
            search: search.unwrap_or_default(),
            page: page.unwrap_or(1),
            ..Browse::default()
        },
        false,
    )
    .await
}

#[poise::command(slash_command)]
#[allow(clippy::too_many_arguments)]
async fn status(
    ctx: Context<'_>,
    #[description = "Only free catalog pricing (default true)"] free_only: Option<bool>,
    #[description = "Filter model ID or name"] search: Option<String>,
    #[description = "Minimum context tokens"] min_context: Option<u32>,
    #[description = "Page number"]
    #[min = 1]
    page: Option<u32>,
    #[description = "Models per page (1–10, default 10)"]
    #[min = 1]
    #[max = 10]
    page_size: Option<u32>,
    #[description = "Short model rows (default false)"] compact: Option<bool>,
    #[description = "Show snapshot to everyone (default false)"] public: Option<bool>,
) -> Result<(), Error> {
    browse(
        ctx,
        Browse {
            free_only: free_only.unwrap_or(true),
            search: search.unwrap_or_default(),
            min_context: min_context.unwrap_or(0),
            page: page.unwrap_or(1),
            page_size: page_size.unwrap_or(10),
            compact: compact.unwrap_or(false),
        },
        public.unwrap_or(false),
    )
    .await
}

#[poise::command(slash_command)]
async fn model(
    ctx: Context<'_>,
    #[description = "Exact model ID from /models or /status"] id: String,
    #[description = "Hide the description (default false)"] compact: Option<bool>,
    #[description = "Include raw pricing (default true)"] show_pricing: Option<bool>,
    #[description = "Show details to everyone (default false)"] public: Option<bool>,
) -> Result<(), Error> {
    let public = public.unwrap_or(false);
    if public {
        ctx.defer().await?;
    } else {
        ctx.defer_ephemeral().await?;
    }
    let found = ctx
        .data()
        .store
        .lock()
        .await
        .state
        .catalog
        .get(&id)
        .cloned();
    let result = match found {
        Some(model) => presentation::model_details(
            &model,
            compact.unwrap_or(false),
            show_pricing.unwrap_or(true),
        ),
        None => embed("Model not found")
            .description("Use /status free_only:false to find an exact model ID."),
    };
    visible_reply(ctx, result, public).await
}

#[derive(Default)]
struct WatchPatch {
    role_id: Option<u64>,
    free_only: Option<bool>,
    search: Option<String>,
    min_context: Option<u32>,
    ping_enabled: Option<bool>,
    compact: Option<bool>,
    clear_role: bool,
}

fn prune_pending(sub: &mut Subscription) {
    let mut seen = BTreeSet::new();
    sub.pending = sub
        .pending
        .iter()
        .filter(|event| {
            sub.matches(&event.model) && seen.insert((event.model.id.clone(), event.kind.clone()))
        })
        .cloned()
        .collect();
}

fn patch_subscription(
    previous: Option<&Subscription>,
    channel_id: u64,
    patch: &WatchPatch,
) -> Result<Subscription, Error> {
    if patch.clear_role && patch.role_id.is_some() {
        return Err("clear_role:true cannot be combined with role.".into());
    }
    let mut sub = previous.cloned().unwrap_or(Subscription {
        channel_id,
        role_id: None,
        free_only: false,
        pending: Vec::new(),
        search: String::new(),
        min_context: 0,
        ping_enabled: false,
        compact: false,
    });
    sub.channel_id = channel_id;
    if patch.clear_role {
        sub.role_id = None;
        sub.ping_enabled = false;
    }
    if let Some(role) = patch.role_id {
        sub.role_id = Some(role);
        sub.ping_enabled = true;
    }
    if let Some(value) = patch.free_only {
        sub.free_only = value;
    }
    if let Some(value) = &patch.search {
        sub.search = value.clone();
    }
    if let Some(value) = patch.min_context {
        sub.min_context = value;
    }
    if let Some(value) = patch.ping_enabled {
        sub.ping_enabled = value;
    }
    if let Some(value) = patch.compact {
        sub.compact = value;
    }
    if sub.ping_enabled && sub.role_id.is_none_or(|id| id == 0) {
        return Err("Pings require a mentionable opt-in role. Use /watch role:@YourRole, or ping_enabled:false.".into());
    }
    prune_pending(&mut sub);
    Ok(sub)
}

fn valid_role(role: &serenity::Role, guild: serenity::GuildId) -> bool {
    role.guild_id == guild
        && role.id.get() != guild.get()
        && role.id.get() != 0
        && role.mentionable
        && !role.managed
}

fn settings(sub: &Subscription, poll_seconds: u64) -> String {
    format!(
        "Channel: <#{}>\nRole: {}\nPings enabled: {}\nScope: {}\nSearch: {}\nMinimum context: {} tokens\nCompact: {}\nQueued alerts: {}\nPoll interval: {poll_seconds} seconds",
        sub.channel_id,
        sub.role_id
            .map(|id| format!("<@&{id}>"))
            .unwrap_or_else(|| "None".into()),
        sub.ping_enabled,
        if sub.free_only {
            "Free models"
        } else {
            "All new models + becoming free"
        },
        text(&sub.search, 500),
        sub.min_context,
        sub.compact,
        sub.pending.len()
    )
}

const WATCH_HELP: &str = "Automatic alerts cover future catalog detections, not historical releases. One channel per server. Omitted /watch options retain settings and matching queued alerts. To enable pings, make an unmanaged opt-in role mentionable in Server Settings → Roles, then run /watch role:@YourRole ping_enabled:true. Use ping_enabled:false to silence pings, clear_role:true to remove the role, search: with a single space to clear the search, min_context:0 to clear the context filter, or /unwatch to stop. /status shows a catalog freshness snapshot, not a live-updating message.";

#[poise::command(
    slash_command,
    guild_only,
    required_permissions = "MANAGE_GUILD",
    default_member_permissions = "MANAGE_GUILD"
)]
#[allow(clippy::too_many_arguments)]
async fn watch(
    ctx: Context<'_>,
    #[description = "Mentionable opt-in notification role; enables pings unless explicitly disabled"]
    role: Option<serenity::Role>,
    #[description = "Only free models (new subscriptions default false; omitted retains setting)"]
    free_only: Option<bool>,
    #[description = "Filter ID or name; a single space clears; omitted retains setting"]
    search: Option<String>,
    #[description = "Minimum context tokens; 0 clears; omitted retains setting"]
    min_context: Option<u32>,
    #[description = "Enable role pings; requires a valid mentionable role"] ping_enabled: Option<
        bool,
    >,
    #[description = "Short notification rows; omitted retains setting"] compact: Option<bool>,
    #[description = "Remove the notification role and disable pings; conflicts with role"]
    clear_role: Option<bool>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = ctx.guild_id().ok_or("Server required")?;
    let patch = WatchPatch {
        role_id: role.as_ref().map(|role| role.id.get()),
        free_only,
        search: search.map(|value| value.trim().to_owned()),
        min_context,
        ping_enabled,
        compact,
        clear_role: clear_role.unwrap_or(false),
    };
    if patch.clear_role && role.is_some() {
        return reply(
            ctx,
            embed("Conflicting settings")
                .description("Choose either clear_role:true or role, not both."),
        )
        .await;
    }
    if role.as_ref().is_some_and(|role| !valid_role(role, guild)) {
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
    let roles = guild.roles(ctx.serenity_context()).await?;
    let mut guard = ctx.data().store.lock().await;
    let sub = match patch_subscription(
        guard.state.subscriptions.get(&guild.get()),
        channel.id.get(),
        &patch,
    ) {
        Ok(sub) => sub,
        Err(error) => {
            drop(guard);
            return reply(
                ctx,
                embed("Invalid alert settings").description(error.to_string()),
            )
            .await;
        }
    };
    if sub.ping_enabled
        && !sub
            .role_id
            .and_then(|id| roles.get(&serenity::RoleId::new(id)))
            .is_some_and(|role| valid_role(role, guild))
    {
        drop(guard);
        return reply(ctx, embed("Choose an opt-in role").description("The configured role is missing or not mentionable/unmanaged. Set role to a valid opt-in role, or use ping_enabled:false or clear_role:true.")).await;
    }
    let result = embed("Alerts configured here")
        .description(WATCH_HELP)
        .field(
            "Complete settings",
            settings(&sub, ctx.data().poll_seconds),
            false,
        );
    let mut next = guard.state.clone();
    next.subscriptions.insert(guild.get(), sub);
    guard.commit(next).await?;
    drop(guard);
    reply(ctx, result).await
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
    ctx.defer_ephemeral().await?;
    let guard = ctx.data().store.lock().await;
    let last = guard
        .state
        .last_success
        .map(|time| format!("<t:{time}:f> (<t:{time}:R>)"))
        .unwrap_or_else(|| "Never".into());
    let mut result = embed("Catalog freshness and alert settings")
        .description("Catalog snapshot only, not uptime or health; not live updating. Use /status to browse the saved catalog.")
        .field("Catalog", guard.state.catalog.len().to_string(), true)
        .field("Free", guard.state.catalog.values().filter(|model| model.is_free()).count().to_string(), true)
        .field("Last successful catalog fetch", last, false)
        .field("Poll interval", format!("{} seconds", ctx.data().poll_seconds), true);
    if let Some(sub) = ctx
        .guild_id()
        .and_then(|id| guard.state.subscriptions.get(&id.get()))
    {
        result = result.field(
            "Complete server settings",
            settings(sub, ctx.data().poll_seconds),
            false,
        );
    } else {
        result = result.field(
            "Server alerts",
            "Not configured here; run /watch in a server text channel.",
            false,
        );
    }
    result = result.field("Configure alerts", WATCH_HELP, false);
    drop(guard);
    reply(ctx, result).await
}

#[poise::command(slash_command)]
async fn about(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let mut result = embed("Model Radar").description(
        "OpenRouter catalog snapshots and automatic opt-in release alerts.\n\n/models — free models; search and page\n/status — snapshot, not live updating or uptime/health; free_only (default true), search, min_context, page, page_size (1–10), compact, public (default false)\n/model — exact ID; compact, show_pricing (default true), public (default false)\n/watch — configure alerts here (Manage Server); role, free_only (default false for new subscriptions), search, min_context, ping_enabled, compact, clear_role; omitted options retain settings\n/testping — send an explicitly labeled TEST to the configured role (Manage Server)\n/unwatch — remove subscription\n/stats — catalog freshness and complete settings\n\nFree to use. No OpenRouter key needed. New means newly observed, not a verified launch date. Pricing and availability can change; provider limits apply. Server/channel/role IDs and filters are stored for alerts; /unwatch deletes the subscription.")
        .field("Configure pings", WATCH_HELP, false)
        .field("Poll interval", format!("{} seconds", ctx.data().poll_seconds), true);
    if let Some(url) = &ctx.data().support_url {
        result = result.field(
            "Optional support",
            format!("[Support the operator]({url}) — donations keep the bot free."),
            false,
        );
    }
    reply(ctx, result).await
}

fn notification_message(sub: &Subscription, count: usize) -> serenity::CreateMessage {
    let description = sub
        .pending
        .iter()
        .take(count.min(8))
        .map(|event| {
            format!(
                "**{}** · {}",
                if event.kind == "paid_to_free" {
                    "Now free"
                } else {
                    "New in catalog"
                },
                presentation::model_line(&event.model, sub.compact)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    role_message(
        sub,
        embed("Model Radar · catalog update").description(if description.is_empty() {
            "No queued detections."
        } else {
            &description
        }),
    )
}

fn role_message(sub: &Subscription, embed: serenity::CreateEmbed) -> serenity::CreateMessage {
    let mut mentions = no_mentions();
    let mut message = serenity::CreateMessage::new().embed(embed);
    if sub.ping_enabled
        && let Some(role) = sub.role_id.filter(|id| *id != 0)
    {
        mentions = mentions.roles([serenity::RoleId::new(role)]);
        message = message.content(format!("<@&{role}>"));
    }
    message.allowed_mentions(mentions)
}

#[poise::command(
    slash_command,
    guild_only,
    required_permissions = "MANAGE_GUILD",
    default_member_permissions = "MANAGE_GUILD"
)]
async fn testping(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = ctx.guild_id().ok_or("Server required")?;
    let snapshot = ctx
        .data()
        .store
        .lock()
        .await
        .state
        .subscriptions
        .get(&guild.get())
        .cloned();
    let Some(sub) = snapshot else {
        return reply(
            ctx,
            embed("No subscription")
                .description("Configure /watch role:@YourRole ping_enabled:true first."),
        )
        .await;
    };
    let roles = guild.roles(ctx.serenity_context()).await?;
    if !sub.ping_enabled
        || !sub
            .role_id
            .and_then(|id| roles.get(&serenity::RoleId::new(id)))
            .is_some_and(|role| valid_role(role, guild))
    {
        return reply(ctx, embed("Pings unavailable").description("Configure /watch role:@YourRole ping_enabled:true with a mentionable opt-in role first.")).await;
    }
    serenity::ChannelId::new(sub.channel_id).send_message(ctx.serenity_context(), role_message(&sub,
        embed("TEST · Model Radar role ping").description("TEST notification only. No new model was detected and no catalog or queued alerts were changed."))).await?;
    reply(
        ctx,
        embed("TEST sent").description(
            "Sent an explicitly labeled TEST role ping to the configured alert channel.",
        ),
    )
    .await
}

fn updated(mut state: State, current: BTreeMap<String, Model>) -> State {
    if state.initialized {
        let events = catalog::changes(&state.catalog, &current);
        for sub in state.subscriptions.values_mut() {
            prune_pending(sub);
            let mut seen: BTreeSet<_> = sub
                .pending
                .iter()
                .map(|event| (event.model.id.clone(), event.kind.clone()))
                .collect();
            let additions: Vec<_> = events
                .iter()
                .filter(|event| {
                    sub.matches(&event.model)
                        && seen.insert((event.model.id.clone(), event.kind.clone()))
                })
                .cloned()
                .collect();
            sub.pending.extend(additions);
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

fn acknowledge(state: &mut State, guild: u64, snapshot: &Subscription, count: usize) -> bool {
    if let Some(current) = state.subscriptions.get_mut(&guild)
        && current == snapshot
    {
        current.pending.drain(..count.min(current.pending.len()));
        true
    } else {
        false
    }
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
        let snapshot = store.lock().await.state.subscriptions.get(&guild).cloned();
        let Some(sub) = snapshot else {
            continue;
        };
        if sub.pending.is_empty() {
            continue;
        }
        let count = sub.pending.len().min(8);
        if serenity::ChannelId::new(sub.channel_id)
            .send_message(http, notification_message(&sub, count))
            .await
            .is_err()
        {
            tracing::warn!(guild, "Alert delivery failed; retained for next poll");
            continue;
        }
        let mut guard = store.lock().await;
        let mut next = guard.state.clone();
        if acknowledge(&mut next, guild, &sub, count) {
            guard.commit(next).await?;
        }
    }
    Ok(())
}

fn parse_poll_seconds(raw: Option<&str>) -> Result<u64, Error> {
    match raw {
        None => Ok(600),
        Some(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|seconds| (60..=86400).contains(seconds))
            .ok_or_else(|| "POLL_SECONDS must be an integer from 60 to 86400".into()),
    }
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
            .any(|ch| ch.is_control() || matches!(ch, '(' | ')' | '<' | '>'))
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
            "Model Radar\n\nRun: DISCORD_TOKEN from your environment; cargo run --locked\nCheck public catalog without Discord: cargo run --locked -- --check\nOptional: POLL_SECONDS (integer 60–86400, default 600), SUPPORT_URL (HTTPS donation link), STATE_PATH (default state.json), DISCORD_GUILD_ID (instant test-server registration).\n\nCreate a bot in Discord Developer Portal. Invite using bot + applications.commands scopes with View Channel, Send Messages, Embed Links. No privileged intents or Administrator permission needed. Keep the token secret; never paste it in chat.\n/models: free models, search/page. /status: catalog freshness snapshot, NOT uptime/health or live updating; free_only (default true), search, min_context, page, page_size (1–10), compact, public (default false). /model: exact ID, compact (default false), show_pricing (default true), public (default false).\n/watch (Manage Server): role, free_only (default false for NEW subscriptions, includes paid releases), search (single space clears), min_context (0 clears), ping_enabled, compact, clear_role. Omitted options preserve settings and matching queued alerts. Setting role enables pings unless ping_enabled:false. clear_role:true removes role and disables pings; cannot be combined with role. Make an unmanaged opt-in role mentionable in Server Settings > Roles, then /watch role:@YourRole ping_enabled:true. Pings require a valid mentionable role. /testping sends a labeled TEST only. /stats shows complete settings; /unwatch removes alerts; /about explains commands.\nOne process per state file. Keep the machine running for 24/7 alerts. State uses atomic writes; failed deliveries retry next poll; a crash or concurrent settings update after sending may duplicate an alert. First fetch is a silent baseline. Catalog reappearances count as new detections; queued events deduplicate model ID + event kind.\nChecks: cargo fmt --check; cargo test; cargo check; cargo clippy --all-targets -- -D warnings"
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg != "--check") {
        return Err("Unknown argument; use --help".into());
    }
    let poll_raw = std::env::var("POLL_SECONDS")
        .map(Some)
        .or_else(|error| match error {
            std::env::VarError::NotPresent => Ok(None),
            other => Err(other),
        })?;
    let poll_seconds = parse_poll_seconds(poll_raw.as_deref())?;
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("model-radar/0.1")
        .build()?;
    if args.iter().any(|arg| arg == "--check") {
        let catalog = catalog::fetch(&http_client).await?;
        println!(
            "Catalog OK: {} models, {} free",
            catalog.len(),
            catalog.values().filter(|model| model.is_free()).count()
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
    let mut commands = vec![
        models(),
        status(),
        model(),
        watch(),
        unwatch(),
        stats(),
        about(),
        testping(),
    ];
    let descriptions = [
        "Browse OpenRouter's free models",
        "Show a catalog freshness snapshot (not live updating)",
        "Inspect a model's details and pricing",
        "Configure automatic release alerts in this channel",
        "Remove this server's alerts",
        "Show catalog freshness and complete alert settings",
        "About Model Radar and optional support",
        "Send an explicitly labeled TEST role ping",
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
                let mut interval = tokio::time::interval(Duration::from_secs(poll_seconds));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    if refresh(&poll_store, &http_client).await.is_err() { tracing::warn!("Catalog refresh failed; preserving previous snapshot"); }
                    if deliver(&poll_store, &http).await.is_err() { tracing::warn!("Saving delivery progress failed; alerts may retry"); }
                }
            });
            tracing::info!(poll_seconds, "Model Radar connected; catalog polling configured");
            Ok(Data { store, support_url, poll_seconds })
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
        let model = Model {
            id: "test/model_under`tick".into(),
            name: "Test".into(),
            description: String::new(),
            context_length: 10,
            pricing: BTreeMap::from([
                ("prompt".into(), if free { "0" } else { "1" }.into()),
                ("completion".into(), "0".into()),
            ]),
        };
        BTreeMap::from([(model.id.clone(), model)])
    }

    fn subscription() -> Subscription {
        patch_subscription(None, 42, &WatchPatch::default()).unwrap()
    }

    fn event(free: bool) -> catalog::Event {
        catalog::Event {
            model: catalog(free).into_values().next().unwrap(),
            kind: "new".into(),
        }
    }

    #[test]
    fn baseline_is_silent_and_alerts_are_filtered_and_deduplicated() {
        let mut state = State::default();
        for (id, free_only) in [(1, true), (2, false)] {
            let mut sub = subscription();
            sub.free_only = free_only;
            state.subscriptions.insert(id, sub);
        }
        let baseline = updated(state, catalog(false));
        assert!(
            baseline
                .subscriptions
                .values()
                .all(|sub| sub.pending.is_empty())
        );
        let free = updated(baseline, catalog(true));
        assert!(
            free.subscriptions
                .values()
                .all(|sub| sub.pending.len() == 1)
        );
        let unchanged = updated(free, catalog(true));
        assert!(
            unchanged
                .subscriptions
                .values()
                .all(|sub| sub.pending.len() == 1)
        );
        let changed = updated(updated(unchanged, catalog(false)), catalog(true));
        assert!(
            changed
                .subscriptions
                .values()
                .all(|sub| sub.pending.len() == 1)
        );
        let removed = updated(changed, BTreeMap::new());
        let returned = updated(removed, catalog(false));
        assert_eq!(returned.subscriptions[&1].pending.len(), 1);
        assert_eq!(returned.subscriptions[&2].pending.len(), 2);
        let again = updated(updated(returned, BTreeMap::new()), catalog(false));
        assert_eq!(again.subscriptions[&2].pending.len(), 2);
    }

    #[test]
    fn patches_preserve_settings_and_pending_and_prune_filters() {
        let mut sub = subscription();
        assert!(!sub.free_only);
        assert!(!sub.ping_enabled);
        sub.role_id = Some(99);
        sub.ping_enabled = true;
        sub.compact = true;
        sub.search = "TEST".into();
        sub.min_context = 5;
        sub.pending = vec![event(false), event(true)];
        let kept = patch_subscription(Some(&sub), 42, &WatchPatch::default()).unwrap();
        assert_eq!(kept.pending.len(), 1);
        sub.pending.truncate(1);
        assert_eq!(kept, sub);
        let moved = patch_subscription(Some(&sub), 43, &WatchPatch::default()).unwrap();
        assert_eq!(moved.channel_id, 43);
        assert_eq!(moved.pending, sub.pending);
        let filtered = patch_subscription(
            Some(&sub),
            42,
            &WatchPatch {
                free_only: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(filtered.pending.is_empty());
        for patch in [
            WatchPatch {
                search: Some("absent".into()),
                ..Default::default()
            },
            WatchPatch {
                min_context: Some(11),
                ..Default::default()
            },
        ] {
            assert!(
                patch_subscription(Some(&sub), 42, &patch)
                    .unwrap()
                    .pending
                    .is_empty()
            );
        }
        let reset = patch_subscription(
            Some(&sub),
            42,
            &WatchPatch {
                search: Some(String::new()),
                min_context: Some(0),
                compact: Some(false),
                clear_role: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(reset.search.is_empty());
        assert_eq!(reset.min_context, 0);
        assert!(!reset.compact);
        assert!(!reset.ping_enabled);
        assert_eq!(reset.role_id, None);
        assert_eq!(reset.pending, sub.pending);
    }

    #[test]
    fn role_patch_rules() {
        assert!(
            patch_subscription(
                None,
                42,
                &WatchPatch {
                    clear_role: true,
                    role_id: Some(99),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            patch_subscription(
                None,
                42,
                &WatchPatch {
                    ping_enabled: Some(true),
                    ..Default::default()
                }
            )
            .is_err()
        );
        let sub = patch_subscription(
            None,
            42,
            &WatchPatch {
                role_id: Some(99),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(sub.ping_enabled);
        let muted = patch_subscription(
            Some(&sub),
            42,
            &WatchPatch {
                role_id: Some(100),
                ping_enabled: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!muted.ping_enabled);
        assert_eq!(muted.role_id, Some(100));
        assert!(
            patch_subscription(
                Some(&sub),
                42,
                &WatchPatch {
                    clear_role: true,
                    ping_enabled: Some(true),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn updated_uses_search_and_context_filters() {
        let mut sub = subscription();
        sub.search = "TEST".into();
        sub.min_context = 10;
        let mut state = State {
            initialized: true,
            ..Default::default()
        };
        state.subscriptions.insert(1, sub.clone());
        sub.search = "missing".into();
        state.subscriptions.insert(2, sub.clone());
        sub.search.clear();
        sub.min_context = 11;
        state.subscriptions.insert(3, sub);
        let state = updated(state, catalog(false));
        assert_eq!(state.subscriptions[&1].pending.len(), 1);
        assert!(state.subscriptions[&2].pending.is_empty());
        assert!(state.subscriptions[&3].pending.is_empty());
    }

    #[test]
    fn acknowledgement_requires_exact_snapshot() {
        let mut sub = subscription();
        sub.pending = vec![event(false)];
        let mut state = State::default();
        state.subscriptions.insert(1, sub.clone());
        let mut changed = sub.clone();
        changed.compact = true;
        assert!(!acknowledge(&mut state, 1, &changed, 1));
        changed = sub.clone();
        changed.pending.push(event(true));
        assert!(!acknowledge(&mut state, 1, &changed, 1));
        assert!(!acknowledge(&mut state, 2, &sub, 1));
        assert_eq!(state.subscriptions[&1].pending.len(), 1);
        assert!(acknowledge(&mut state, 1, &sub, 1));
        assert!(state.subscriptions[&1].pending.is_empty());
    }

    #[test]
    fn notification_payload_only_allows_opt_in_role() {
        let mut sub = subscription();
        sub.role_id = Some(99);
        sub.pending = vec![event(false)];
        sub.pending[0].model.name = "@everyone <@123> <@&456>".into();
        for enabled in [false, true] {
            sub.ping_enabled = enabled;
            let payload = serde_json::to_value(notification_message(&sub, 8)).unwrap();
            let mentions = &payload["allowed_mentions"];
            assert_eq!(mentions["parse"], serde_json::json!([]));
            assert_eq!(mentions["replied_user"], false);
            assert!(mentions["users"].as_array().is_none_or(Vec::is_empty));
            if enabled {
                assert_eq!(payload["content"], "<@&99>");
                assert_eq!(mentions["roles"], serde_json::json!(["99"]));
            } else {
                assert!(payload["content"].as_str().is_none_or(str::is_empty));
                assert!(mentions["roles"].as_array().is_none_or(Vec::is_empty));
            }
        }
        sub.role_id = None;
        let payload = serde_json::to_value(notification_message(&sub, 8)).unwrap();
        assert!(payload["content"].as_str().is_none_or(str::is_empty));
    }

    #[test]
    fn notification_and_settings_payloads_are_bounded() {
        let mut sub = subscription();
        sub.search = "\u{1d11e}_`".repeat(2000);
        sub.channel_id = u64::MAX;
        sub.role_id = Some(u64::MAX);
        sub.min_context = u32::MAX;
        let mut event = event(false);
        event.model.name = sub.search.clone();
        event.model.id = sub.search.clone();
        event.model.context_length = u64::MAX;
        sub.pending = vec![event; 30];
        for compact in [false, true] {
            sub.compact = compact;
            let payload = serde_json::to_value(notification_message(&sub, usize::MAX)).unwrap();
            presentation::tests::assert_embed_bounds(&payload["embeds"][0]);
        }
        let payload = serde_json::to_value(embed("Settings").description(WATCH_HELP).field(
            "Complete settings",
            settings(&sub, 86400),
            false,
        ))
        .unwrap();
        presentation::tests::assert_embed_bounds(&payload);
        let setting = payload["fields"][0]["value"].as_str().unwrap();
        for label in [
            "Channel:",
            "Role:",
            "Pings enabled:",
            "Scope:",
            "Search:",
            "Minimum context:",
            "Compact:",
            "Queued alerts:",
            "Poll interval:",
        ] {
            assert!(setting.contains(label));
        }
        let payload = serde_json::to_value(role_message(
            &sub,
            embed("TEST · Model Radar role ping").description("TEST only; no new model detected."),
        ))
        .unwrap();
        assert!(
            payload["embeds"][0]["title"]
                .as_str()
                .unwrap()
                .starts_with("TEST")
        );
        presentation::tests::assert_embed_bounds(&payload["embeds"][0]);
        let mentions = serde_json::to_value(no_mentions()).unwrap();
        assert_eq!(mentions["parse"], serde_json::json!([]));
        assert!(mentions["roles"].as_array().is_none_or(Vec::is_empty));
        assert!(mentions["users"].as_array().is_none_or(Vec::is_empty));
    }

    #[test]
    fn public_and_private_replies_never_allow_mentions() {
        for public in [false, true] {
            let reply = response(embed("@everyone <@123> <@&99>"), public);
            let initial = serde_json::to_value(
                reply
                    .clone()
                    .to_slash_initial_response(serenity::CreateInteractionResponseMessage::new()),
            )
            .unwrap();
            assert_eq!(
                initial["flags"].as_u64().unwrap_or_default() & 64 != 0,
                !public
            );
            let edit = serde_json::to_value(
                reply
                    .clone()
                    .to_slash_initial_response_edit(serenity::EditInteractionResponse::new()),
            )
            .unwrap();
            let followup = serde_json::to_value(
                reply
                    .to_slash_followup_response(serenity::CreateInteractionResponseFollowup::new()),
            )
            .unwrap();
            for payload in [initial, edit, followup] {
                assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
                assert_eq!(payload["allowed_mentions"]["roles"], serde_json::json!([]));
                assert_eq!(payload["allowed_mentions"]["users"], serde_json::json!([]));
                assert_eq!(payload["allowed_mentions"]["replied_user"], false);
            }
        }
    }

    #[test]
    fn poll_interval_validation() {
        assert_eq!(parse_poll_seconds(None).unwrap(), 600);
        for value in ["60", "600", "86400"] {
            assert_eq!(
                parse_poll_seconds(Some(value)).unwrap(),
                value.parse::<u64>().unwrap()
            );
        }
        for value in [
            "",
            "0",
            "59",
            "86401",
            "-60",
            "60.0",
            "hello",
            "18446744073709551616",
        ] {
            assert!(parse_poll_seconds(Some(value)).is_err());
        }
    }
}

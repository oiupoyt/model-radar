mod catalog;
mod store;

use poise::serenity_prelude as serenity;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, (), Error>;

#[poise::command(slash_command)]
async fn ping(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Model Radar is online.").await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let token = std::env::var("DISCORD_TOKEN")?;
    let mut ping = ping();
    ping.description = Some("Check whether Model Radar is online".into());
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![ping],
            ..Default::default()
        })
        .setup(|ctx, _, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                Ok(())
            })
        })
        .build();
    serenity::ClientBuilder::new(token, serenity::GatewayIntents::GUILDS)
        .framework(framework)
        .await?
        .start()
        .await?;
    Ok(())
}

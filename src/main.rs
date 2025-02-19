use color_eyre::eyre::{bail, Result};
use derive_config::DeriveTomlConfig;
use serde::{Deserialize, Serialize};
use serenity::{
    all::{
        ActivityData,
        ActivityType,
        ChannelId,
        GuildId,
        OnlineStatus,
        PermissionOverwrite,
        PermissionOverwriteType,
        Ready,
        VoiceState,
    },
    async_trait,
    model::Permissions,
    prelude::*,
    Client,
};

#[derive(Clone, Default, DeriveTomlConfig, Deserialize, Serialize)]
#[serde(default)] /* Default new fields instead of overwriting */
struct Config {
    /// Discord token
    token: String,

    /// Discord guild id
    guild: GuildId,

    /// The public voice channel id which gives users access to the video channel when joining
    voice: ChannelId,

    /// The public video channel id which users are given access to when joining the voice channel
    video: ChannelId,
}

impl TypeMapKey for Config {
    type Value = Self;
}

struct Events;

#[async_trait]
impl EventHandler for Events {
    async fn ready(&self, ctx: Context, data_about_bot: Ready) {
        println!("Ready: {}", data_about_bot.user.name);

        let activity = ActivityData {
            name:  String::from("with fire"),
            kind:  ActivityType::Playing,
            state: None,
            url:   None,
        };

        ctx.set_presence(Some(activity), OnlineStatus::Online);
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        #[allow(clippy::significant_drop_tightening)]
        let data = ctx.data.read().await;
        let Some(config) = data.get::<Config>() else {
            return;
        };

        let Some(member) = new.member else {
            return;
        };

        let Some(guild_id) = new.guild_id else {
            return;
        };

        if guild_id != config.guild {
            return;
        }

        if let Some(new_channel_id) = new.channel_id {
            /* User joined the voice channel, giving view permission */
            if new_channel_id == config.voice {
                println!("[{}] joined the voice channel", member.display_name());
                println!("Giving access to the video channel");

                let target = PermissionOverwrite {
                    allow: Permissions::VIEW_CHANNEL,
                    deny:  Permissions::empty(),
                    kind:  PermissionOverwriteType::Member(new.user_id),
                };

                if let Err(error) = config.video.create_permission(&ctx, target).await {
                    eprintln!("Error updating channel permissions: {error}");
                };
            }

            /* User started streaming in the voice channel */
            if let Some(stream) = new.self_stream {
                if stream && new_channel_id == config.voice {
                    let result = guild_id.move_member(&ctx, new.user_id, config.video).await;
                    if let Err(error) = result {
                        eprintln!("Error moving channel: {error}");
                    };
                }
            }
        }

        let Some(old) = old else {
            return;
        };

        let Some(old_channel_id) = old.channel_id else {
            return;
        };

        /* User left the voice/video channel, remove view permission */
        let channels = [config.voice, config.video];
        if channels.contains(&old_channel_id) {
            if let Some(new_channel_id) = new.channel_id {
                if channels.contains(&new_channel_id) {
                    return;
                }
            }

            println!("[{}] left the video channel", member.display_name());
            println!("Removing access to the video channel");

            let permission_type = PermissionOverwriteType::Member(new.user_id);
            let result = config.video.delete_permission(&ctx, permission_type).await;

            if let Err(error) = result {
                eprintln!("Error updating channel permissions: {error}");
            };
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let config = Config::load().unwrap_or_default();
    if config.token.is_empty() {
        eprintln!("You must provide a Discord token in the config file");
        config.save()?;
        return Ok(());
    }

    let intents = GatewayIntents::non_privileged();
    let mut client = Client::builder(&config.token, intents)
        .event_handler(Events)
        .await?;

    client.data.write().await.insert::<Config>(config);

    println!("Starting...");
    client.start().await?;
    bail!("Unreachable")
}

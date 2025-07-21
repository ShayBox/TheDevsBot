use color_eyre::eyre::{bail, Result};
use derive_config::DeriveTomlConfig;
use serde::{Deserialize, Serialize};
use serenity::{
    all::{
        ActivityData,
        ActivityType,
        ChannelId,
        Command,
        CommandInteraction,
        CreateCommand,
        CreateInteractionResponse,
        CreateInteractionResponseMessage,
        GuildId,
        Interaction,
        OnlineStatus,
        PermissionOverwrite,
        PermissionOverwriteType,
        Ready,
        RoleId,
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

    /// Discord guild ID
    guild: GuildId,

    /// The public voice channel ID, which gives users access to the video channel when joining.
    voice: ChannelId,

    /// The public video channel ID, which users are given access to when joining the voice channel.
    video: ChannelId,

    /// The alerts role ID that users can add/remove with the /alerts command.
    alerts: RoleId,
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
            name:  String::from("with commands"),
            kind:  ActivityType::Playing,
            state: None,
            url:   None,
        };

        ctx.set_presence(Some(activity), OnlineStatus::Online);

        let command =
            CreateCommand::new("alerts").description("Toggle the alerts role for yourself");

        match Command::create_global_command(&ctx.http, command).await {
            Ok(_) => println!("Successfully registered /alerts command"),
            Err(error) => eprintln!("Error creating command: {error}"),
        }
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
                    }
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
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            if let Err(error) = handle_command(&ctx, &command).await {
                eprintln!("Error handling command: {error}");
            }
        }
    }
}

async fn handle_command(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    #[allow(clippy::single_match_else)]
    match command.data.name.as_str() {
        "alerts" => handle_alerts_command(ctx, command).await?,
        _ => {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("Unknown command")
                    .ephemeral(true),
            );

            command.create_response(&ctx.http, response).await?;
        }
    }
    Ok(())
}

async fn handle_alerts_command(ctx: &Context, command: &CommandInteraction) -> Result<()> {
    let data = ctx.data.read().await;
    let Some(config) = data.get::<Config>() else {
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("Configuration not found")
                .ephemeral(true),
        );
        command.create_response(&ctx.http, response).await?;
        return Ok(());
    };

    let Some(guild_id) = command.guild_id else {
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("This command can only be used in a guild")
                .ephemeral(true),
        );
        command.create_response(&ctx.http, response).await?;
        return Ok(());
    };

    if guild_id != config.guild {
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("This command is not available in this guild")
                .ephemeral(true),
        );
        command.create_response(&ctx.http, response).await?;
        return Ok(());
    }

    if config.alerts.get() == 0 {
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("Alerts role is not configured. Please contact an administrator.")
                .ephemeral(true),
        );
        command.create_response(&ctx.http, response).await?;
        return Ok(());
    }

    let member = guild_id.member(&ctx.http, command.user.id).await?;
    let has_role = member.roles.contains(&config.alerts);

    let (message, success) = if has_role {
        match member.remove_role(&ctx.http, config.alerts).await {
            Ok(()) => ("Successfully removed the alerts role!", true),
            Err(_) => (
                "Failed to remove the alerts role. Please contact an administrator.",
                false,
            ),
        }
    } else {
        match member.add_role(&ctx.http, config.alerts).await {
            Ok(()) => ("Successfully added the alerts role!", true),
            Err(_) => (
                "Failed to add the alerts role. Please contact an administrator.",
                false,
            ),
        }
    };

    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(message)
            .ephemeral(true),
    );
    command.create_response(&ctx.http, response).await?;

    if success {
        let action = if has_role { "removed" } else { "added" };
        println!("[{}] {} the alerts role", command.user.name, action);
    }

    Ok(())
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

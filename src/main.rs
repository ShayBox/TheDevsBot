use std::{
    fs,
    io,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{bail, eyre, Result};
use derive_config::DeriveTomlConfig;
use rand::{rng, seq::IndexedRandom, Rng};
use serde::{Deserialize, Serialize};
use serenity::{
    all::{
        ActivityData,
        ActivityType,
        ChannelId,
        Command,
        CommandInteraction,
        CreateAttachment,
        CreateCommand,
        CreateInteractionResponse,
        CreateInteractionResponseMessage,
        EditGuild,
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
use tokio::time::{sleep, Duration};

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

    /// Path to a directory of images that will be used when randomizing the server icon.
    server_icons_unused: PathBuf,

    /// Path to a directory of images that have already been used as server icons.
    server_icons_used: PathBuf,

    /// Minimum randomized delay (hours) before applying a new server icon.
    server_icons_delay_min_hours: u64,

    /// Maximum randomized delay (hours) before applying a new server icon.
    server_icons_delay_max_hours: u64,
}

impl TypeMapKey for Config {
    type Value = Self;
}

fn is_supported_icon(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    )
}

fn load_icon_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    if directory.as_os_str().is_empty() {
        return Ok(Vec::new());
    }

    let metadata = match fs::metadata(directory) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
            return Ok(Vec::new());
        }
        Err(error) => bail!(
            "Failed to access server icon path '{}': {error}",
            directory.display()
        ),
    };

    if !metadata.is_dir() {
        bail!(
            "Server icon path '{}' is not a directory",
            directory.display()
        );
    }

    let read_dir = fs::read_dir(directory)
        .map_err(|error| eyre!("Failed to read '{directory:?}': {error}"))?;

    let mut paths = Vec::new();
    for entry in read_dir {
        let path = entry?.path();
        if path.is_file() && is_supported_icon(&path) {
            paths.push(path);
        }
    }

    Ok(paths)
}

fn icon_filename(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| eyre!("Server icon path '{path:?}' is missing a filename"))
}

fn move_icon_file(source: &Path, target_dir: &Path) -> Result<PathBuf> {
    let filename = icon_filename(source)?;
    let destination = target_dir.join(&filename);

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    if destination.exists() {
        fs::remove_file(&destination)?;
    }

    if let Err(rename_error) = fs::rename(source, &destination) {
        fs::copy(source, &destination).and_then(|_| fs::remove_file(source)).map_err(
            |copy_error| eyre!(
                "Failed to move icon from '{}' to '{}': rename error: {rename_error}; copy error: {copy_error}",
                source.display(),
                destination.display()
            ),
        )?;
    }

    Ok(destination)
}

fn recycle_used_icons(unused_dir: &Path, used_dir: &Path) -> Result<Vec<PathBuf>> {
    let used_paths = load_icon_paths(used_dir)?;
    if used_paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut moved = Vec::new();
    for path in used_paths {
        let destination = move_icon_file(&path, unused_dir)?;
        moved.push(destination);
    }

    Ok(moved)
}

fn icon_delay(min_hours: u64, max_hours: u64) -> Result<Option<Duration>> {
    if max_hours == 0 {
        return Ok(None);
    }

    if min_hours > max_hours {
        bail!(
            "server_icon_delay_min_hours ({}) cannot be greater than server_icon_delay_max_hours ({})",
            min_hours,
            max_hours
        );
    }

    let hours = if min_hours == max_hours {
        min_hours
    } else {
        let mut rng = rng();
        rng.random_range(min_hours..=max_hours)
    };

    if hours == 0 {
        Ok(None)
    } else {
        let seconds = hours
            .checked_mul(3_600)
            .ok_or_else(|| eyre!("Server icon delay is too large"))?;
        Ok(Some(Duration::from_secs(seconds)))
    }
}

async fn next_icon_delay(ctx: &Context) -> Result<Option<Duration>> {
    let data = ctx.data.read().await;
    let Some(config) = data.get::<Config>() else {
        return Ok(None);
    };

    icon_delay(
        config.server_icons_delay_min_hours,
        config.server_icons_delay_max_hours,
    )
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

        if let Err(error) = randomize_server_icon(&ctx).await {
            eprintln!("Error randomizing server icon: {error}");
        }

        let ctx_clone = ctx.clone();
        tokio::spawn(async move {
            loop {
                let Some(delay) = (match next_icon_delay(&ctx_clone).await {
                    Ok(delay) => delay,
                    Err(error) => {
                        eprintln!("Error calculating server icon delay: {error}");
                        break;
                    }
                }) else {
                    println!("Server icon delay disabled; stopping icon randomizer loop");
                    break;
                };

                println!(
                    "Waiting {:?} before updating server icon (range {}-{} hours)",
                    delay,
                    ctx_clone
                        .data
                        .read()
                        .await
                        .get::<Config>()
                        .map(|config| config.server_icons_delay_min_hours)
                        .unwrap_or_default(),
                    ctx_clone
                        .data
                        .read()
                        .await
                        .get::<Config>()
                        .map(|config| config.server_icons_delay_max_hours)
                        .unwrap_or_default()
                );

                sleep(delay).await;

                if let Err(error) = randomize_server_icon(&ctx_clone).await {
                    eprintln!("Error randomizing server icon: {error}");
                }
            }
        });
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

async fn randomize_server_icon(ctx: &Context) -> Result<()> {
    let (guild_id, unused_dir, used_dir) = {
        let data = ctx.data.read().await;
        let Some(config) = data.get::<Config>() else {
            return Ok(());
        };

        if config.server_icons_unused.as_os_str().is_empty() {
            return Ok(());
        }

        (
            config.guild,
            config.server_icons_unused.clone(),
            config.server_icons_used.clone(),
        )
    };

    let mut icon_paths = match load_icon_paths(&unused_dir) {
        Ok(paths) => paths,
        Err(error) => {
            let io_denied = error
                .downcast_ref::<io::Error>()
                .is_some_and(|io_error| io_error.kind() == io::ErrorKind::PermissionDenied);

            if io_denied {
                eprintln!(
                    "Server icon path '{}' is not readable: {error}",
                    unused_dir.display()
                );
                return Ok(());
            }

            return Err(error);
        }
    };

    if icon_paths.is_empty() {
        println!(
            "Server icon directory '{}' is empty, recycling used icons from '{}'",
            unused_dir.display(),
            used_dir.display()
        );

        icon_paths = match recycle_used_icons(&unused_dir, &used_dir) {
            Ok(paths) => paths,
            Err(error) => {
                let io_denied = error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|io_error| io_error.kind() == io::ErrorKind::PermissionDenied);

                if io_denied {
                    eprintln!(
                        "Server icon path '{}' is not readable: {error}",
                        used_dir.display()
                    );
                    return Ok(());
                }

                return Err(error);
            }
        };
    }

    if icon_paths.is_empty() {
        println!(
            "Server icon directory '{}' is empty or contains no supported images",
            unused_dir.display()
        );
        return Ok(());
    }

    let selected_icon = {
        let mut rng = rng();
        icon_paths
            .choose(&mut rng)
            .cloned()
            .ok_or_else(|| eyre!("Failed to select a server icon"))?
    };

    let icon_name = icon_filename(&selected_icon)?;
    let attachment = CreateAttachment::path(&selected_icon).await?;
    let builder = EditGuild::new().icon(Some(&attachment));

    guild_id.edit(&ctx.http, builder).await?;
    move_icon_file(&selected_icon, &used_dir)?;
    println!(
        "Updated server icon to '{}' from '{}'",
        icon_name,
        selected_icon.display()
    );

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

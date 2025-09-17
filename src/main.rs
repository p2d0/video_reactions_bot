use std::sync::Arc;
use teloxide::{prelude::*, types::*, utils::command::BotCommands};
use tokio::sync::Mutex;

// --- Data Structures for Storing Video Information ---

#[derive(Clone, Debug)]
struct VideoData {
    caption: String,
    file_id: String,
}

type SharedState = Arc<Mutex<Vec<VideoData>>>;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "These commands are supported:")]
enum Command {
    #[command(description = "Displays this help message.")]
    Help,
    #[command(description = "Start a dialog to remove a saved video.")]
    Remove,
}

// --- Main Bot Logic ---

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting video saver bot...");

    let bot = Bot::from_env();
    let state: SharedState = Arc::new(Mutex::new(Vec::new()));

    let handler = dptree::entry()
        .branch(Update::filter_message().filter_command::<Command>().endpoint(handle_command))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query))
        .branch(Update::filter_message().endpoint(handle_message));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

/// Handler for bot commands like `/help` and `/remove`.
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: SharedState,
) -> Result<(), teloxide::RequestError> {
    match cmd {
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string()).await?;
        }
        Command::Remove => {
            let videos = state.lock().await;

            if videos.is_empty() {
                bot.send_message(msg.chat.id, "You have no saved videos to remove.").await?;
                return Ok(());
            }

            let mut keyboard_buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
            for video in videos.iter() {
                // MODIFIED: Truncate the file_id to fit within the 64-byte limit.
                // We take the first 50 characters, which is safe and unique enough.
                let mut short_file_id = video.file_id.clone();
                short_file_id.truncate(50);

                let button = InlineKeyboardButton::callback(
                    video.caption.clone(),
                    format!("delete_{}", short_file_id),
                );
                keyboard_buttons.push(vec![button]);
            }

            let keyboard = InlineKeyboardMarkup::new(keyboard_buttons);

            bot.send_message(msg.chat.id, "Select a video to remove:")
                .reply_markup(keyboard)
                .await?;
        }
    }
    Ok(())
}

/// Handler for callback queries (when a user presses an inline button).
async fn handle_callback_query(
    bot: Bot,
    q: CallbackQuery,
    state: SharedState,
) -> Result<(), teloxide::RequestError> {
    if let Some(data) = q.data {
        // MODIFIED: We look for a prefix "delete_" and then use the rest of the data
        // to find a file_id that STARTS WITH that prefix.
        if let Some(file_id_prefix_to_delete) = data.strip_prefix("delete_") {
            let mut videos = state.lock().await;
            let mut caption_of_deleted = String::new();

            videos.retain(|video| {
                if video.file_id.starts_with(file_id_prefix_to_delete) {
                    caption_of_deleted = video.caption.clone();
                    false // Do not keep this element (i.e., delete it)
                } else {
                    true // Keep this element
                }
            });

            bot.answer_callback_query(q.id).await?;

            if let Some(message) = q.message {
                if !caption_of_deleted.is_empty() {
                    bot.edit_message_text(
                        message.chat.id,
                        message.id,
                        format!("✅ Video '{}' has been removed.", caption_of_deleted),
                    )
                    .await?;
                } else {
                    bot.edit_message_text(message.chat.id, message.id, "Video not found. It might have been already removed.")
                        .await?;
                }
            }
        }
    }
    Ok(())
}

/// Handler for regular messages to save videos.
async fn handle_message(
    bot: Bot,
    msg: Message,
    state: SharedState,
) -> Result<(), teloxide::RequestError> {
    if let (Some(video), Some(caption)) = (msg.video(), msg.caption()) {
        let mut videos = state.lock().await;
        let new_video_data = VideoData {
            caption: caption.to_string(),
            file_id: video.file.id.clone(),
        };
        log::info!("Storing new video: {:?}", new_video_data);
        videos.push(new_video_data);
        bot.send_message(msg.chat.id, "✅ Video and caption saved successfully!")
            .reply_to_message_id(msg.id)
            .await?;
    } else {
        bot.send_message(
            msg.chat.id,
            "Please send me a video with a caption to save it. Use /help to see available commands.",
        )
        .await?;
    }
    Ok(())
}

/// Handler for inline queries.
async fn handle_inline_query(
    bot: Bot,
    q: InlineQuery,
    state: SharedState,
) -> Result<(), teloxide::RequestError> {
    let query_text = &q.query;
    log::info!("Received inline query: '{}'", query_text);

    let videos = state.lock().await;
    let results: Vec<InlineQueryResult> = videos
        .iter()
        .filter(|video| {
            video.caption.to_lowercase().contains(&query_text.to_lowercase())
        })
        .enumerate()
        .map(|(i, video)| {
            let inline_video = InlineQueryResultCachedVideo::new(
                i.to_string(),
                video.file_id.clone(),
                video.caption.clone(),
            )
            .caption(video.caption.clone());

            InlineQueryResult::CachedVideo(inline_video)
        })
        .collect();

    if let Err(e) = bot.answer_inline_query(&q.id, results).await {
        log::error!("Error answering inline query: {:?}", e);
    }

    Ok(())
}

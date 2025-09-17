use std::sync::Arc;
use teloxide::{prelude::*, types::*, utils::command::BotCommands};
use sqlx::SqlitePool; // Use SqlitePool for our shared state

// MODIFIED: This struct now derives `sqlx::FromRow` so we can easily
// map database query results directly into this struct.
#[derive(Clone, Debug, sqlx::FromRow)]
struct VideoData {
    caption: String,
    file_id: String,
}

// MODIFIED: The shared state is now a connection pool to our SQLite database.
// This is thread-safe and designed for async environments.
type SharedState = SqlitePool;

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

    // NEW: Load environment variables from .env file
    dotenv::dotenv().expect("Failed to read .env file");

    let bot = Bot::from_env();
    let path = "sqlite:videos.db";

    // NEW: Set up the database connection pool
    let pool = SqlitePool::connect(&path)
        .await
        .expect("Failed to connect to database");

    // NEW: Run database migrations (in this case, just creating the table)
    // This ensures the `videos` table exists before the bot starts handling updates.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS videos (
            file_id TEXT PRIMARY KEY NOT NULL,
            caption TEXT NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await
    .expect("Failed to create database table");


    let handler = dptree::entry()
        .branch(Update::filter_message().filter_command::<Command>().endpoint(handle_command))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query))
        .branch(Update::filter_message().endpoint(handle_message));

    Dispatcher::builder(bot, handler)
        // MODIFIED: We now inject the database pool into our handlers.
        .dependencies(dptree::deps![pool])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

/// Handler for regular messages to save videos.
async fn handle_message(
    bot: Bot,
    msg: Message,
    pool: SharedState, // Receive the database pool
) -> Result<(), teloxide::RequestError> {
    let mut video_to_save: Option<&Video> = None;
    let mut caption_to_save: Option<&str> = None;

    if let (Some(video), Some(caption)) = (msg.video(), msg.caption()) {
        video_to_save = Some(video);
        caption_to_save = Some(caption);
    }
    else if let (Some(reply), Some(caption)) = (msg.reply_to_message(), msg.text()) {
        if let Some(video) = reply.video() {
            video_to_save = Some(video);
            caption_to_save = Some(caption);
        }
    }

    if let (Some(video), Some(caption)) = (video_to_save, caption_to_save) {
        let file_id = &video.file.id;

        // MODIFIED: Insert the new video into the database.
        // `INSERT OR IGNORE` will do nothing if a video with the same file_id (PRIMARY KEY)
        // already exists, preventing duplicates gracefully.
        let result = sqlx::query("INSERT OR IGNORE INTO videos (file_id, caption) VALUES (?, ?)")
            .bind(file_id)
            .bind(caption)
            .execute(&pool)
            .await;

        match result {
            Ok(_) => {
                log::info!("Stored video: {} - {}", file_id, caption);
                 bot.send_message(msg.chat.id, "✅ Video and caption saved successfully!")
                    .reply_to_message_id(msg.id)
                    .await?;
            }
            Err(e) => {
                log::error!("Failed to save video to DB: {:?}", e);
                bot.send_message(msg.chat.id, "❌ Failed to save video due to a database error.")
                    .reply_to_message_id(msg.id)
                    .await?;
            }
        }
    }
    else {
        bot.send_message(
            msg.chat.id,
            "To save a video, either:\n\
            1. Send a video with a caption.\n\
            2. Reply to a video with the caption you want to use.\n\n\
            Use /help to see other commands.",
        )
        .await?;
    }
    Ok(())
}

/// Handler for the `/remove` command.
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    pool: SharedState,
) -> Result<(), teloxide::RequestError> {
    if let Command::Remove = cmd {
        // MODIFIED: Fetch all videos from the database.
        let videos: Vec<VideoData> = sqlx::query_as("SELECT file_id, caption FROM videos")
            .fetch_all(&pool)
            .await
            .unwrap_or_default(); // If there's an error, treat it as an empty list.

        if videos.is_empty() {
            bot.send_message(msg.chat.id, "You have no saved videos to remove.").await?;
            return Ok(());
        }

        let mut keyboard_buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
        for video in videos {
            let mut short_file_id = video.file_id;
            short_file_id.truncate(50);
            let button = InlineKeyboardButton::callback(
                video.caption,
                format!("delete_{}", short_file_id),
            );
            keyboard_buttons.push(vec![button]);
        }

        let keyboard = InlineKeyboardMarkup::new(keyboard_buttons);
        bot.send_message(msg.chat.id, "Select a video to remove:")
            .reply_markup(keyboard)
            .await?;
    }
    Ok(())
}

/// Handler for callback queries (deleting a video).
async fn handle_callback_query(
    bot: Bot,
    q: CallbackQuery,
    pool: SharedState,
) -> Result<(), teloxide::RequestError> {
    if let Some(data) = q.data {
        if let Some(file_id_prefix) = data.strip_prefix("delete_") {
            // MODIFIED: Delete the video from the database.
            // We use `LIKE` because we only have a prefix of the file_id.
            let query_pattern = format!("{}%", file_id_prefix);

            // First, get the caption of the video we are about to delete for the confirmation message.
            let video_to_delete: Option<VideoData> = sqlx::query_as("SELECT file_id, caption FROM videos WHERE file_id LIKE ?")
                .bind(&query_pattern)
                .fetch_optional(&pool)
                .await
                .unwrap_or(None);

            if let Some(video) = video_to_delete {
                // Now, execute the deletion.
                sqlx::query("DELETE FROM videos WHERE file_id = ?")
                    .bind(&video.file_id)
                    .execute(&pool)
                    .await
                    .ok(); // Ignore potential errors for simplicity.

                bot.answer_callback_query(q.id).await?;
                if let Some(message) = q.message {
                    bot.edit_message_text(
                        message.chat.id,
                        message.id,
                        format!("✅ Video '{}' has been removed.", video.caption),
                    ).await?;
                }
            } else {
                 bot.answer_callback_query(q.id).await?;
                 if let Some(message) = q.message {
                    bot.edit_message_text(message.chat.id, message.id, "Video not found. It might have been already removed.").await?;
                 }
            }
        }
    }
    Ok(())
}

/// Handler for inline queries.
async fn handle_inline_query(
    bot: Bot,
    q: InlineQuery,
    pool: SharedState,
) -> Result<(), teloxide::RequestError> {
    let query_text = &q.query;

    // MODIFIED: Fetch videos from the database based on the search query.
    let videos: Vec<VideoData> = if query_text.is_empty() {
        // If query is empty, get all videos.
        sqlx::query_as("SELECT file_id, caption FROM videos")
            .fetch_all(&pool)
            .await
            .unwrap_or_default()
    } else {
        // If there's a query, search using `LIKE`.
        let search_pattern = format!("%{}%", query_text);
        sqlx::query_as("SELECT file_id, caption FROM videos WHERE caption LIKE ?")
            .bind(search_pattern)
            .fetch_all(&pool)
            .await
            .unwrap_or_default()
    };

    let results: Vec<InlineQueryResult> = videos
        .into_iter()
        .enumerate()
        .map(|(i, video)| {
            let inline_video = InlineQueryResultCachedVideo::new(
                i.to_string(),
                video.file_id,
                video.caption.clone(),
            ).caption(video.caption);
            InlineQueryResult::CachedVideo(inline_video)
        })
        .collect();

    if let Err(e) = bot.answer_inline_query(&q.id, results).await {
        log::error!("Error answering inline query: {:?}", e);
    }
    Ok(())
}

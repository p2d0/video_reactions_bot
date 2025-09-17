use std::sync::Arc;
use teloxide::{prelude::*, types::*};
use tokio::sync::Mutex;
use log;

// --- Data Structures for Storing Video Information ---

// A single video entry, linking a caption to a Telegram file_id.
// We use `#[derive(Clone)]` so we can easily copy it.
#[derive(Clone, Debug)]
struct VideoData {
    // The caption provided by the user. This is what we will search.
    caption: String,
    // The unique ID Telegram assigns to the video file.
    // We use this to send the video without re-uploading.
    file_id: String,
}

// The shared state of our bot.
// It will hold a list of all the videos users have sent.
// We wrap it in Arc<Mutex<...>> to allow safe, shared access
// across multiple concurrent tasks (e.g., handling multiple messages at once).
type SharedState = Arc<Mutex<Vec<VideoData>>>;

// --- Main Bot Logic ---

#[tokio::main]
async fn main() {
    // Initialize the logger for easier debugging.
    pretty_env_logger::init();
    log::info!("Starting video saver bot...");

    // Create a new bot instance from the `TELOXIDE_TOKEN` environment variable.
    let bot = Bot::from_env();

    // Initialize our in-memory storage for videos.
    let state: SharedState = Arc::new(Mutex::new(Vec::new()));

    // Set up the command and message handlers using a dispatcher.
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query));

    Dispatcher::builder(bot, handler)
        // Inject our shared state into the handler chain.
        // This makes the `state` available to all our handler functions.
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

/// Handler for regular messages sent to the bot.
async fn handle_message(
    bot: Bot,
    msg: Message,
    state: SharedState,
) -> Result<(), teloxide::RequestError> {
    // We only care about messages that contain both a video and a caption.
    if let (Some(video), Some(caption)) = (msg.video(), msg.caption()) {
        // Lock the state so we can safely add the new video.
        // The lock is released automatically when `videos` goes out of scope.
        let mut videos = state.lock().await;

        // Create a new VideoData entry.
        let new_video_data = VideoData {
            caption: caption.to_string(),
            file_id: video.file.id.clone(),
        };

        log::info!("Storing new video: {:?}", new_video_data);

        // Add the new video to our list.
        videos.push(new_video_data);

        // Confirm to the user that the video has been saved.
        bot.send_message(msg.chat.id, "✅ Video and caption saved successfully!")
            .reply_to_message_id(msg.id)
            .await?;
    } else {
        // If the message is not a video with a caption, send a help message.
        bot.send_message(
            msg.chat.id,
            "Please send me a video with a caption to save it.",
        )
        .await?;
    }

    Ok(())
}

/// Handler for inline queries (e.g., `@YourBotName some search`).
async fn handle_inline_query(
    bot: Bot,
    q: InlineQuery,
    state: SharedState,
) -> Result<(), teloxide::RequestError> {
    // The user's search text.
    let query_text = &q.query;
    log::info!("Received inline query: '{}'", query_text);

    // Lock the state to safely read the video list.
    let videos = state.lock().await;

    // Filter the videos based on the search query.
    // We convert both the caption and the query to lowercase for case-insensitive search.
    let results: Vec<InlineQueryResult> = videos
        .iter()
        .filter(|video| {
            video.caption.to_lowercase().contains(&query_text.to_lowercase())
        })
        .enumerate() // Use enumerate to get a unique ID for each result.
        .map(|(i, video)| {
            // We use `InlineQueryResult::CachedVideo` because the video is already on
            // Telegram's servers. This is much more efficient than re-uploading.
            let inline_video = InlineQueryResultCachedVideo::new(
                i.to_string(),          // A unique ID for this result.
                video.file_id.clone(),  // The file_id of the video.
                video.caption.clone(),  // The title to be displayed in the results list.
            );

            // The `caption` field inside the result will be the text sent along with the video.
            let inline_video_with_caption = inline_video.caption(video.caption.clone());

            InlineQueryResult::CachedVideo(inline_video_with_caption)
        })
        .collect();

    // Answer the inline query with the list of results.
    // If `results` is empty, Telegram will show "No results found".
    if let Err(e) = bot.answer_inline_query(&q.id, results).await {
        log::error!("Error answering inline query: {:?}", e);
    }

    Ok(())
}

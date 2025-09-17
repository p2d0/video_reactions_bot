use teloxide::{prelude::*, types::*, utils::command::BotCommands};
use sqlx::SqlitePool;
use tempfile::Builder;
use tokio::fs;
use teloxide::net::Download;

use image::io::Reader as ImageReader;
use imageproc::contours::{find_contours, Contour};
use imageproc::rect::Rect;
use std::path::Path;

// --- Data Structures (unchanged) ---

#[derive(Clone, Debug, sqlx::FromRow)]
struct VideoData { caption: String, file_id: String, }
type SharedState = SqlitePool;
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "These commands are supported:")]
enum Command {
    #[command(description = "Displays this help message.")]
    Help,
    #[command(description = "Start a dialog to remove a saved video.")]
    Remove,
    #[command(description = "Reply to a video to replace the top text banner.")]
    Edit(String),
}

// --- Computer Vision Logic (Corrected Calculation) ---
#[derive(Debug, Clone, Copy)]
struct BoundingBox { x: i32, y: i32, w: u32, h: u32, }

fn detect_white_box(image_path: &Path) -> Option<BoundingBox> {
    let img = ImageReader::open(image_path).ok()?.decode().ok()?;
    let gray_image = imageproc::map::map_pixels(&img.to_rgb8(), |_, _, p| {
        if p[0] > 240 && p[1] > 240 && p[2] > 240 { image::Luma([255]) } else { image::Luma([0]) }
    });
    let contours: Vec<Contour<i32>> = find_contours(&gray_image);

    let largest_box = contours.iter()
        .filter(|c| !c.points.is_empty())
        .map(|contour| {
            let mut min_x = i32::MAX;
            let mut min_y = i32::MAX;
            let mut max_x = i32::MIN;
            let mut max_y = i32::MIN;
            for point in &contour.points {
                min_x = min_x.min(point.x);
                min_y = min_y.min(point.y);
                max_x = max_x.max(point.x);
                max_y = max_y.max(point.y);
            }
            // FIXED: Added +1 to width and height to prevent zero-sized rectangles.
            Rect::at(min_x, min_y).of_size((max_x - min_x + 1) as u32, (max_y - min_y + 1) as u32)
        })
        .filter(|rect| rect.width() > 50 && rect.height() > 20)
        .max_by_key(|rect| rect.width() * rect.height());

    largest_box.map(|rect| BoundingBox {
        x: rect.left(), y: rect.top(), w: rect.width(), h: rect.height(),
    })
}

// --- Main Bot Logic (The rest of the file is unchanged) ---

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting video saver bot...");
    dotenv::dotenv().expect("Failed to read .env file");
    let bot = Bot::from_env();
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = SqlitePool::connect(&database_url).await.expect("Failed to connect to database");
    sqlx::query(r#"CREATE TABLE IF NOT EXISTS videos (file_id TEXT PRIMARY KEY NOT NULL, caption TEXT NOT NULL)"#)
        .execute(&pool).await.expect("Failed to create database table");
    let handler = dptree::entry()
        .branch(Update::filter_message().filter_command::<Command>().endpoint(handle_command))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query))
        .branch(Update::filter_message().endpoint(handle_message));
    Dispatcher::builder(bot, handler).dependencies(dptree::deps![pool]).enable_ctrlc_handler().build().dispatch().await;
}

async fn edit_video_text(bot: Bot, msg: Message, text_parts: String) -> Result<(), teloxide::RequestError> {
    let replied_to = match msg.reply_to_message() {
        Some(msg) => msg,
        None => { bot.send_message(msg.chat.id, "Please reply to a video message with the `/edit` command.").await?; return Ok(()); }
    };
    let video = match replied_to.video() {
        Some(video) => video,
        None => { bot.send_message(msg.chat.id, "The message you replied to does not contain a video.").await?; return Ok(()); }
    };

    let status_msg = bot.send_message(msg.chat.id, "Analyzing video... 🧐").await?;

    let temp_dir = Builder::new().prefix("video_edit").tempdir().unwrap();
    let temp_dir_path = temp_dir.path();
    let input_path = temp_dir_path.join("input.mp4");
    let output_path = temp_dir_path.join("output.mp4");
    let frame_path = temp_dir_path.join("frame.png");

    let file = bot.get_file(&video.file.id).await?;
    let mut dest = fs::File::create(&input_path).await.unwrap();
    bot.download_file(&file.path, &mut dest).await?;

    let frame_extraction_status = tokio::process::Command::new("ffmpeg")
        .arg("-i").arg(&input_path).arg("-vframes").arg("1").arg(&frame_path).status().await.ok();
    if frame_extraction_status.is_none() || !frame_extraction_status.unwrap().success() {
        bot.edit_message_text(status_msg.chat.id, status_msg.id, "❌ Failed to extract a frame from the video.").await?;
        return Ok(());
    }

    let detected_box = detect_white_box(&frame_path);
    let (box_x, box_y, box_w, box_h) = match detected_box {
        Some(b) => (b.x.to_string(), b.y.to_string(), b.w.to_string(), b.h.to_string()),
        None => ("0".to_string(), "0".to_string(), "iw".to_string(), "150".to_string()),
    };

    bot.edit_message_text(status_msg.chat.id, status_msg.id, "Processing video... ⏳").await?;

    let font_path = std::env::var("FONT_PATH").expect("FONT_PATH must be set in .env");
    let parts: Vec<&str> = text_parts.split("//").map(|s| s.trim()).collect();
    let filter_complex = if parts.len() == 3 {
        format!(
            "[0:v]drawbox=x={x}:y={y}:w={w}:h={h}:color=white:t=fill, \
             drawtext=fontfile='{font}':text='{t1}':fontcolor=black:fontsize=48:x={x}+(({w}-text_w)/2):y={y}+(({h}-text_h)/2):enable='lt(t,{ts})', \
             drawtext=fontfile='{font}':text='{t2}':fontcolor=black:fontsize=48:x={x}+(({w}-text_w)/2):y={y}+(({h}-text_h)/2):enable='gte(t,{ts})'[v]",
             font = font_path, t1 = parts[0].replace('\'', r"\'"), t2 = parts[2].replace('\'', r"\'"), ts = parts[1],
             x=box_x, y=box_y, w=box_w, h=box_h
        )
    } else {
        format!(
            "[0:v]drawbox=x={x}:y={y}:w={w}:h={h}:color=white:t=fill, \
             drawtext=fontfile='{font}':text='{txt}':fontcolor=black:fontsize=48:x={x}+(({w}-text_w)/2):y={y}+(({h}-text_h)/2)[v]",
             font = font_path, txt = text_parts.replace('\'', r"\'"),
             x=box_x, y=box_y, w=box_w, h=box_h
        )
    };

    let mut command = tokio::process::Command::new("ffmpeg");
    command.arg("-i").arg(&input_path).arg("-filter_complex").arg(filter_complex)
        .arg("-map").arg("[v]").arg("-map").arg("0:a?").arg("-c:a").arg("copy").arg(&output_path);

    if command.status().await.is_ok_and(|s| s.success()) {
        bot.edit_message_text(status_msg.chat.id, status_msg.id, "Uploading edited video... 🚀").await?;
        bot.send_video(msg.chat.id, InputFile::file(&output_path)).await?;
        bot.delete_message(status_msg.chat.id, status_msg.id).await?;
    } else {
        bot.edit_message_text(status_msg.chat.id, status_msg.id, "❌ An error occurred during video processing.").await?;
    }

    Ok(())
}

async fn handle_command(bot: Bot, msg: Message, cmd: Command, pool: SharedState) -> Result<(), teloxide::RequestError> {
    match cmd {
        Command::Help => { bot.send_message(msg.chat.id, Command::descriptions().to_string()).await?; }
        Command::Remove => {
            let videos: Vec<VideoData> = sqlx::query_as("SELECT file_id, caption FROM videos").fetch_all(&pool).await.unwrap_or_default();
            if videos.is_empty() { bot.send_message(msg.chat.id, "You have no saved videos to remove.").await?; return Ok(()); }
            let keyboard_buttons: Vec<Vec<_>> = videos.into_iter().map(|video| {
                let mut short_id = video.file_id; short_id.truncate(50);
                vec![InlineKeyboardButton::callback(video.caption, format!("delete_{}", short_id))]
            }).collect();
            bot.send_message(msg.chat.id, "Select a video to remove:").reply_markup(InlineKeyboardMarkup::new(keyboard_buttons)).await?;
        }
        Command::Edit(text) => { if let Err(e) = edit_video_text(bot, msg, text).await { log::error!("Error during video editing: {:?}", e); } }
    }
    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, pool: SharedState) -> Result<(), teloxide::RequestError> {
    let mut video_to_save: Option<&Video> = None; let mut caption_to_save: Option<&str> = None;
    if let (Some(video), Some(caption)) = (msg.video(), msg.caption()) { video_to_save = Some(video); caption_to_save = Some(caption);
    } else if let (Some(reply), Some(caption)) = (msg.reply_to_message(), msg.text()) { if let Some(video) = reply.video() { video_to_save = Some(video); caption_to_save = Some(caption); } }
    if let (Some(video), Some(caption)) = (video_to_save, caption_to_save) {
        if sqlx::query("INSERT OR IGNORE INTO videos (file_id, caption) VALUES (?, ?)").bind(&video.file.id).bind(caption).execute(&pool).await.is_ok() {
            bot.send_message(msg.chat.id, "✅ Video saved!").await?;
        } else { bot.send_message(msg.chat.id, "❌ DB error.").await?; }
    } else { bot.send_message(msg.chat.id, "Send a video with a caption or reply to a video to save it.").await?; }
    Ok(())
}

async fn handle_callback_query(bot: Bot, q: CallbackQuery, pool: SharedState) -> Result<(), teloxide::RequestError> {
    if let Some(data) = q.data {
        if let Some(prefix) = data.strip_prefix("delete_") {
            let pattern = format!("{}%", prefix);
            if let Some(video) = sqlx::query_as::<_, VideoData>("SELECT file_id, caption FROM videos WHERE file_id LIKE ?")
                .bind(&pattern).fetch_optional(&pool).await.unwrap_or(None) {
                    sqlx::query("DELETE FROM videos WHERE file_id = ?").bind(&video.file_id).execute(&pool).await.ok();
                    bot.answer_callback_query(q.id).await?;
                    if let Some(m) = q.message { bot.edit_message_text(m.chat.id, m.id, format!("✅ Removed '{}'", video.caption)).await?; }
            }
        }
    }
    Ok(())
}

async fn handle_inline_query(bot: Bot, q: InlineQuery, pool: SharedState) -> Result<(), teloxide::RequestError> {
    let query_text = &q.query;
    let videos: Vec<VideoData> = if query_text.is_empty() { sqlx::query_as("SELECT file_id, caption FROM videos").fetch_all(&pool).await.unwrap_or_default()
    } else { let pattern = format!("%{}%", query_text); sqlx::query_as("SELECT file_id, caption FROM videos WHERE caption LIKE ?").bind(pattern).fetch_all(&pool).await.unwrap_or_default() };
    let results = videos.into_iter().enumerate().map(|(i, video)| {
        InlineQueryResult::CachedVideo(InlineQueryResultCachedVideo::new(i.to_string(), video.file_id, video.caption.clone()).caption(video.caption))
    });
    bot.answer_inline_query(&q.id, results).await?;
    Ok(())
}

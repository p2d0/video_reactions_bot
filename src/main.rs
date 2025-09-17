use teloxide::{prelude::*, types::*, utils::command::BotCommands};
use sqlx::SqlitePool;
use tempfile::Builder;
use tokio::fs;
use teloxide::net::Download;
use std::path::Path;

// Imports for Base64 encoding and inline editing
use base64::{engine::general_purpose, Engine as _};
use image::io::Reader as ImageReader;
use imageproc::{contours::{find_contours, Contour}, rect::Rect};

// --- Data Structures ---

#[derive(Clone, Debug, sqlx::FromRow)]
struct VideoData { caption: String, file_id: String }
type SharedState = SqlitePool;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "These commands are supported:")]
enum Command {
    #[command(description = "Displays this help message.")]
    Help,
    #[command(description = "Start a dialog to remove a saved video.")]
    Remove,
}

// --- Computer Vision Logic ---
#[derive(Debug, Clone, Copy)]
struct BoundingBox { x: i32, y: i32, w: u32, h: u32 }

fn detect_white_box(image_path: &Path) -> Option<BoundingBox> {
    let img = ImageReader::open(image_path).ok()?.decode().ok()?;
    let gray_image = imageproc::map::map_pixels(&img.to_rgb8(), |_, _, p| {
        if p[0] > 240 && p[1] > 240 && p[2] > 240 { image::Luma([255]) } else { image::Luma([0]) }
    });
    let contours: Vec<Contour<i32>> = find_contours(&gray_image);
    let largest_box = contours.iter().filter(|c| !c.points.is_empty()).map(|contour| {
        let mut min_x = i32::MAX; let mut min_y = i32::MAX; let mut max_x = i32::MIN; let mut max_y = i32::MIN;
        for point in &contour.points {
            min_x = min_x.min(point.x); min_y = min_y.min(point.y); max_x = max_x.max(point.x); max_y = max_y.max(point.y);
        }
        Rect::at(min_x, min_y).of_size((max_x - min_x + 1) as u32, (max_y - min_y + 1) as u32)
    }).filter(|rect| rect.width() > 50 && rect.height() > 20).max_by_key(|rect| rect.width() * rect.height());
    largest_box.map(|rect| BoundingBox { x: rect.left(), y: rect.top(), w: rect.width(), h: rect.height() })
}

// --- Main Bot Logic ---

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
        .branch(Update::filter_chosen_inline_result().endpoint(handle_chosen_inline_result))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query))
        .branch(Update::filter_message().endpoint(handle_message));

    Dispatcher::builder(bot, handler).dependencies(dptree::deps![pool]).enable_ctrlc_handler().build().dispatch().await;
}

// --- Background Video Editing Task ---

async fn perform_video_edit(bot: Bot, user_id: UserId, inline_message_id: String, file_id: String, text_parts: String) {
    let temp_dir = match Builder::new().prefix("video_edit").tempdir() {
        Ok(dir) => dir,
        Err(e) => { log::error!("Failed to create temp dir: {}", e); return; }
    };
    let temp_dir_path = temp_dir.path();
    let input_path = temp_dir_path.join("input.mp4");
    let output_path = temp_dir_path.join("output.mp4");
    let frame_path = temp_dir_path.join("frame.png");

    let Ok(file) = bot.get_file(&file_id).await else {
        bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not get video file info.").await.ok();
        return
    };
    let Ok(mut dest) = fs::File::create(&input_path).await else {
        bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not create temporary file.").await.ok();
        return
    };
    if bot.download_file(&file.path, &mut dest).await.is_err() {
        bot.edit_message_text_inline(&inline_message_id, "❌ Error: Failed to download video.").await.ok();
        return
    };

    let frame_extraction_status = tokio::process::Command::new("ffmpeg")
        .arg("-i").arg(&input_path).arg("-vframes").arg("1").arg(&frame_path).status().await.ok();
    if frame_extraction_status.is_none() || !frame_extraction_status.unwrap().success() {
        bot.edit_message_text_inline(&inline_message_id, "❌ Error: Failed to extract frame from video.").await.ok();
        return;
    }

    let detected_box = detect_white_box(&frame_path);
    let (box_x, box_y, box_w, box_h) = match detected_box {
        Some(b) => (b.x.to_string(), b.y.to_string(), b.w.to_string(), b.h.to_string()),
        None => ("0".to_string(), "0".to_string(), "iw".to_string(), "150".to_string()),
    };

    let font_path = std::env::var("FONT_PATH").expect("FONT_PATH must be set in .env");
    let parts: Vec<&str> = text_parts.split("//").map(|s| s.trim()).collect();
    let filter_complex = if parts.len() == 3 {
        format!("[0:v]drawbox=x={x}:y={y}:w={w}:h={h}:color=white:t=fill, drawtext=fontfile='{font}':text='{t1}':fontcolor=black:fontsize=48:x={x}+(({w}-text_w)/2):y={y}+(({h}-text_h)/2):enable='lt(t,{ts})', drawtext=fontfile='{font}':text='{t2}':fontcolor=black:fontsize=48:x={x}+(({w}-text_w)/2):y={y}+(({h}-text_h)/2):enable='gte(t,{ts})'[v]",
            font=font_path, t1=parts[0].replace('\'',r"\'"), t2=parts[2].replace('\'',r"\'"), ts=parts[1], x=box_x, y=box_y, w=box_w, h=box_h)
    } else {
        format!("[0:v]drawbox=x={x}:y={y}:w={w}:h={h}:color=white:t=fill, drawtext=fontfile='{font}':text='{txt}':fontcolor=black:fontsize=48:x={x}+(({w}-text_w)/2):y={y}+(({h}-text_h)/2)[v]",
            font=font_path, txt=text_parts.replace('\'',r"\'"), x=box_x, y=box_y, w=box_w, h=box_h)
    };

    let mut command = tokio::process::Command::new("ffmpeg");
    command.arg("-i").arg(&input_path).arg("-filter_complex").arg(filter_complex)
        .arg("-map").arg("[v]").arg("-map").arg("0:a?").arg("-c:a").arg("copy").arg(&output_path);

    if command.status().await.is_ok_and(|s| s.success()) {
        let temp_message = match bot.send_video(user_id, InputFile::file(&output_path)).await {
            Ok(msg) => msg,
            Err(_) => { bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not pre-upload video.").await.ok(); return; }
        };

        let new_video_file_id = match temp_message.video() {
            Some(vid) => vid.file.id.clone(),
            None => { bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not get new file_id.").await.ok(); return; }
        };

        bot.delete_message(user_id, temp_message.id).await.ok();

        let media = InputMedia::Video(InputMediaVideo::new(InputFile::file_id(new_video_file_id)));

        if bot.edit_message_media_inline(&inline_message_id, media).await.is_err() {
            log::warn!("Failed to edit inline message with video. It might have been deleted.");
        }
    } else {
        bot.edit_message_text_inline(&inline_message_id, "❌ An error occurred during video processing.").await.ok();
    }
}


// --- Bot Handlers ---

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
    }
    Ok(())
}

async fn handle_chosen_inline_result(bot: Bot, chosen: ChosenInlineResult, pool: SharedState) -> Result<(), teloxide::RequestError> {
    let Some(inline_message_id) = chosen.inline_message_id else {
        log::warn!("ChosenInlineResult is missing an inline_message_id. This shouldn't happen.");
        return Ok(());
    };

    if let Some(payload) = chosen.result_id.strip_prefix("edit_") {
        if let Some((file_id_prefix, b64_text)) = payload.rsplit_once('_') {
            let pattern = format!("{}%", file_id_prefix);

            if let Some(video) = sqlx::query_as::<_, VideoData>("SELECT file_id, caption FROM videos WHERE file_id LIKE ?")
                .bind(pattern).fetch_optional(&pool).await.unwrap_or(None) {

                if let Ok(text_bytes) = general_purpose::STANDARD.decode(b64_text) {
                    if let Ok(new_text) = String::from_utf8(text_bytes) {
                        let user_id = chosen.from.id;
                        tokio::spawn(perform_video_edit(
                            bot.clone(),
                            user_id,
                            inline_message_id,
                            video.file_id,
                            new_text,
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

// MODIFIED: This function now contains the new, more advanced parsing logic for time-based edits.
async fn handle_inline_query(bot: Bot, q: InlineQuery, pool: SharedState) -> Result<(), teloxide::RequestError> {
    let mut results = vec![];

    if let Some((search_term, edit_params_raw)) = q.query.split_once("/edit") {
        let edit_params = edit_params_raw.trim();
        let mut final_edit_text = String::new();
        let mut display_description = String::new();

        // --- NEW Parsing Logic ---
        // Try to parse the format: `message1 /time message2`
        if let Some((msg1, rest)) = edit_params.rsplit_once('/') {
            if let Some((time_str, msg2)) = rest.trim().split_once(' ') {
                if let Ok(time) = time_str.parse::<f64>() {
                    // It's a valid time-based edit
                    let final_msg1 = msg1.trim();
                    let final_msg2 = msg2.trim();
                    final_edit_text = format!("{} // {} // {}", final_msg1, time, final_msg2);
                    display_description = format!("TEXT 1: '{}' | TEXT 2: '{}' (at {}s)", final_msg1, final_msg2, time);
                }
            }
        }

        // If the parsing logic above didn't produce a result, it's a single-message edit.
        if final_edit_text.is_empty() {
            final_edit_text = edit_params.to_string();
            display_description = format!("Click to replace text with: '{}'", edit_params);
        }
        // --- End of NEW Parsing Logic ---

        let search_pattern = format!("%{}%", search_term.trim());
        if let Some(video) = sqlx::query_as::<_, VideoData>("SELECT file_id, caption FROM videos WHERE caption LIKE ?")
            .bind(search_pattern).fetch_optional(&pool).await.unwrap_or(None) {

                let b64_text = general_purpose::STANDARD.encode(&final_edit_text);

                let mut file_id_prefix = video.file_id.clone();
                file_id_prefix.truncate(30);
                let result_id = format!("edit_{}_{}", file_id_prefix, b64_text);

                if result_id.len() <= 64 {
                    let dummy_keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("⚙️ Processing...", "ignore")]]);

                    let result = InlineQueryResult::CachedVideo(
                        InlineQueryResultCachedVideo::new(result_id, video.file_id, format!("EDIT: {}", video.caption))
                        .description(display_description)
                        .input_message_content(InputMessageContent::Text(
                            InputMessageContentText::new("⚙️ Preparing your video...")
                        ))
                        .reply_markup(dummy_keyboard)
                    );
                    results.push(result);
                }
            }
    } else {
        let videos: Vec<VideoData> = if q.query.is_empty() { sqlx::query_as("SELECT file_id, caption FROM videos").fetch_all(&pool).await.unwrap_or_default()
        } else { let pattern = format!("%{}%", q.query); sqlx::query_as("SELECT file_id, caption FROM videos WHERE caption LIKE ?").bind(pattern).fetch_all(&pool).await.unwrap_or_default() };

        results = videos.into_iter().enumerate().map(|(i, video)| {
            InlineQueryResult::CachedVideo(InlineQueryResultCachedVideo::new(i.to_string(), video.file_id, video.caption.clone()).caption(video.caption))
        }).collect();
    }

    bot.answer_inline_query(q.id, results).await?;
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
        if data == "ignore" {
            bot.answer_callback_query(q.id).await?;
            return Ok(());
        }

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

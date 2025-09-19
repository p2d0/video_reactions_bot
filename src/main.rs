use teloxide::{prelude::*, types::*, utils::command::BotCommands};
use sqlx::SqlitePool;
use tempfile::Builder;
use tokio::fs;
use teloxide::net::Download;
use std::path::Path;
use std::cmp::Reverse;
use std::env;

// Imports for computer vision and inline editing.
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

// MODIFIED: This function now detects white boxes, and if none are found, falls back to detecting black boxes.
fn detect_white_or_black_boxes(image_path: &Path) -> Vec<BoundingBox> {
    let Some(img) = ImageReader::open(image_path).ok().and_then(|r| r.decode().ok()) else { return vec![]; };

    // First, convert the whole image to grayscale
    let gray_image = img.to_luma8();

    // --- 1. Attempt to find WHITE boxes ---
    let white_binary_image = imageproc::map::map_pixels(&gray_image, |_, _, p| {
        if p[0] > 250 { image::Luma([255]) } else { image::Luma([0]) }
    });

    let contours: Vec<Contour<i32>> = find_contours(&white_binary_image);

    let mut boxes: Vec<Rect> = contours.iter().filter(|c| !c.points.is_empty()).map(|contour| {
        let mut min_x = i32::MAX; let mut min_y = i32::MAX; let mut max_x = i32::MIN; let mut max_y = i32::MIN;
        for point in &contour.points {
            min_x = min_x.min(point.x); min_y = min_y.min(point.y); max_x = max_x.max(point.x); max_y = max_y.max(point.y);
        }
        Rect::at(min_x, min_y).of_size((max_x - min_x + 1) as u32, (max_y - min_y + 1) as u32)
    }).filter(|rect| rect.width() > 50 && rect.height() > 50).collect();

    // --- 2. If no white boxes were found, search for BLACK boxes ---
    if boxes.is_empty() {
        // Apply a threshold to find very dark regions, making them white for contour detection.
        let black_binary_image = imageproc::map::map_pixels(&gray_image, |_, _, p| {
            if p[0] < 1 { image::Luma([255]) } else { image::Luma([0]) }
        });

        let black_contours: Vec<Contour<i32>> = find_contours(&black_binary_image);

        boxes = black_contours.iter().filter(|c| !c.points.is_empty()).map(|contour| {
            let mut min_x = i32::MAX; let mut min_y = i32::MAX; let mut max_x = i32::MIN; let mut max_y = i32::MIN;
            for point in &contour.points {
                min_x = min_x.min(point.x); min_y = min_y.min(point.y); max_x = max_x.max(point.x); max_y = max_y.max(point.y);
            }
            Rect::at(min_x, min_y).of_size((max_x - min_x + 1) as u32, (max_y - min_y + 1) as u32)
        }).filter(|rect| rect.width() > 50 && rect.height() > 50).collect();
    }

    // --- 3. Sort and take the largest 2 boxes found (either white or black) ---
    boxes.sort_by_key(|b| Reverse(b.width() * b.height()));

    boxes.into_iter().take(2).map(|rect| BoundingBox {
        x: rect.left(), y: rect.top(), w: rect.width(), h: rect.height(),
    }).collect()
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

// --- Background Video Editing Task ---

use std::path::PathBuf; // Add this to your imports at the top

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

    let Ok(file) = bot.get_file(&file_id).await else { return };
    let Ok(mut dest) = fs::File::create(&input_path).await else { return };
    if bot.download_file(&file.path, &mut dest).await.is_err() { return };

    let ffprobe_output = match tokio::process::Command::new("ffprobe")
        .arg("-v").arg("error")
        .arg("-select_streams").arg("v:0")
        .arg("-show_entries").arg("stream=width,height")
        .arg("-of").arg("csv=p=0:s=x")
        .arg(&input_path)
        .output().await {
            Ok(out) => out,
            Err(e) => {
                log::error!("ffprobe failed: {}", e);
                bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not analyze video dimensions.").await.ok();
                return;
            }
        };

    let dims: Vec<u32> = String::from_utf8(ffprobe_output.stdout).unwrap_or_default().trim()
        .split('x').filter_map(|s| s.parse().ok()).collect();
    let (width, height) = if dims.len() == 2 { (dims[0], dims[1]) } else { (0,0) };
    if width == 0 || height == 0 {
        bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not determine video dimensions.").await.ok();
        return;
    }


    let frame_extraction_status = tokio::process::Command::new("ffmpeg")
        .arg("-i").arg(&input_path).arg("-vframes").arg("1").arg(&frame_path).status().await.ok();
    if frame_extraction_status.is_none() || !frame_extraction_status.unwrap().success() {
        bot.edit_message_text_inline(&inline_message_id, "❌ Error: Failed to extract frame.").await.ok();
        return;
    }

    let detected_boxes = detect_white_or_black_boxes(&frame_path);
    let messages: Vec<&str> = text_parts.split("///").collect();
    // --- UPDATED: Use the new universal font path ---
    let font_path_str = std::env::var("UNIVERSAL_FONT_PATH").expect("UNIVERSAL_FONT_PATH must be set in .env");
    let font_path = PathBuf::from(&font_path_str);
    let font_name = font_path.file_stem().and_then(|s| s.to_str()).unwrap_or("Noto Sans");


    let final_filter_chain: String;
    let final_map_tag = "[v_out]";

    if detected_boxes.is_empty() {
        // --- NEW LOGIC: NO BOXES FOUND -> Use Subtitles Filter for Wrapping ---
        let full_text = messages.join("\\N").trim().to_string(); // Join with \N for hard breaks in ASS format
        if full_text.is_empty() {
             bot.edit_message_text_inline(&inline_message_id, "❌ Error: No text provided to add to video.").await.ok();
             return;
        }

        let pad_height = (height as f32 * 0.15).max(100.0) as u32;
        let font_size = (pad_height as f32 * 0.4).max(30.0) as u32;
        let v_margin = (pad_height as f32 * 0.25) as u32;

        let ass_content = format!(
            r#"[Script Info]
Title: Default Aegisub file
ScriptType: v4.00+
WrapStyle: 0
PlayResX: {width}
PlayResY: {height}

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Caption,{font_name},{font_size},&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,1,8,10,10,{v_margin},1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,9:59:59.99,Caption,,0,0,0,,{text}"#,
            width = width,
            height = height + pad_height,
            font_name = font_name,
            font_size = font_size,
            v_margin = v_margin,
            text = full_text
        );

        let ass_path = temp_dir_path.join("subs.ass");
        if tokio::fs::write(&ass_path, ass_content).await.is_err() {
            bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not write temporary subtitle file.").await.ok();
            return;
        }

        // FFmpeg requires path separators to be escaped.
        let escaped_ass_path = ass_path.to_string_lossy().replace('\\', "/");

        final_filter_chain = format!(
            "[0:v]pad=width=in_w:height=in_h+{pad}:x=0:y={pad}:color=black, subtitles=filename='{subs_path}', format=yuv420p[v_out]",
            pad = pad_height,
            subs_path = escaped_ass_path
        );
    } else {
        // --- EXISTING LOGIC: BOXES FOUND -> Overwrite them (now using the universal font) ---
        let mut filter_parts = vec![];
        let mut last_tag = "[0:v]".to_string();
        let boxes_to_edit = detected_boxes.iter().take(messages.len());

        for (i, bbox) in boxes_to_edit.enumerate() {
            let text_to_draw = messages.get(i).unwrap_or(&messages[0]).trim();
            let sanitized_text = text_to_draw.replace('\'', r"\'");
            let current_tag = format!("[v{}]", i);
            let font_size = (bbox.h as f32 * 0.6).max(24.0) as u32;

            let filter = format!(
                "{last_tag}drawbox=x={x}:y={y}:w={w}:h={h}:color=white:t=fill, \
                 drawtext=fontfile='{font}':text='{txt}':fontcolor=black:fontsize={fs}:x={x}+(({w}-text_w)/2):y={y}+(({h}-text_h)/2){out}",
                last_tag = &last_tag,
                x=bbox.x, y=bbox.y, w=bbox.w, h=bbox.h,
                font=font_path_str, // Use the string path here
                txt=sanitized_text,
                fs=font_size,
                out=&current_tag
            );
            filter_parts.push(filter);
            last_tag = current_tag;
        }

        let final_format_filter = format!("{last_tag}format=yuv420p[v_out]", last_tag = &last_tag);
        filter_parts.push(final_format_filter);
        final_filter_chain = filter_parts.join(";");
    }

    let mut command = tokio::process::Command::new("ffmpeg");
    command.arg("-i").arg(&input_path).arg("-filter_complex").arg(&final_filter_chain)
        .arg("-map").arg(final_map_tag).arg("-map").arg("0:a?").arg("-c:a").arg("copy");

    let encoder = env::var("FFMPEG_ENCODER").unwrap_or_default();
    if !encoder.is_empty() {
        log::info!("Using hardware encoder: {}", &encoder);
        command.arg("-c:v").arg(&encoder);
    } else if env::var("CUDA_ENABLED").is_ok() {
        log::info!("Using NVIDIA CUDA encoder: h264_nvenc");
        command.arg("-c:v").arg("h264_nvenc").arg("-preset").arg("p7").arg("-rc").arg("vbr").arg("-gpu").arg("0");
    } else {
        log::info!("Using CPU encoder with 'ultrafast' preset.");
        command.arg("-c:v").arg("libx264").arg("-preset").arg("ultrafast");
    }
    command.arg("-flags").arg("+global_header").arg("-movflags").arg("+faststart").arg("-pix_fmt").arg("yuv420p").arg(&output_path);

    if command.status().await.is_ok_and(|s| s.success()) {
        let temp_message = match bot.send_video(user_id, InputFile::file(&output_path)).await {
            Ok(msg) => msg,
            Err(_) => { bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not pre-upload video.").await.ok(); return; }
        };
        let new_video_file_id = match temp_message.video() { Some(vid) => vid.file.id.clone(), None => return };
        bot.delete_message(user_id, temp_message.id).await.ok();
        let media = InputMedia::Video(InputMediaVideo::new(InputFile::file_id(new_video_file_id)));
        if bot.edit_message_media_inline(&inline_message_id, media).await.is_err() {
            log::warn!("Failed to edit inline message.");
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
    let Some(inline_message_id) = chosen.inline_message_id else { return Ok(()) };

    if let Some(file_id_prefix) = chosen.result_id.strip_prefix("edit_") {
        let pattern = format!("{}%", file_id_prefix);
        if let Some(video) = sqlx::query_as::<_, VideoData>("SELECT file_id, caption FROM videos WHERE file_id LIKE ?")
            .bind(pattern).fetch_optional(&pool).await.unwrap_or(None) {

            if let Some((_, edit_params_raw)) = chosen.query.split_once("/edit") {
                let edit_params = edit_params_raw.trim();
                let final_edit_text = if let Some((msg1, msg2)) = edit_params.split_once("/box2") {
                    format!("{}///{}", msg1.trim(), msg2.trim())
                } else {
                    edit_params.to_string()
                };

                let user_id = chosen.from.id;
                tokio::spawn(perform_video_edit(
                    bot.clone(), user_id, inline_message_id, video.file_id, final_edit_text,
                ));
            }
        }
    }
    Ok(())
}

async fn handle_inline_query(bot: Bot, q: InlineQuery, pool: SharedState) -> Result<(), teloxide::RequestError> {
    const PAGE_SIZE: i64 = 30;
    let page: i64 = q.offset.parse().unwrap_or(0);
    let sql_offset = page * PAGE_SIZE;

    let mut results = vec![];

    if let Some((search_term, edit_params_raw)) = q.query.split_once("/edit") {
        let edit_params = edit_params_raw.trim();
        let display_description = if let Some((msg1, msg2)) = edit_params.split_once("/box2") {
            format!("BOX 1: '{}' | BOX 2: '{}'", msg1.trim(), msg2.trim())
        } else {
            format!("Click to replace text with: '{}'", edit_params)
        };

        let search_pattern = format!("%{}%", search_term.trim());
        if let Some(video) = sqlx::query_as::<_, VideoData>("SELECT file_id, caption FROM videos WHERE caption LIKE ? LIMIT 1")
            .bind(search_pattern).fetch_optional(&pool).await.unwrap_or(None) {

                let mut file_id_prefix = video.file_id.clone();
                file_id_prefix.truncate(55);
                let result_id = format!("edit_{}", file_id_prefix);
                let dummy_keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("⚙️ Processing...", "ignore")]]);

                let result = InlineQueryResult::CachedVideo(
                    InlineQueryResultCachedVideo::new(result_id, video.file_id, format!("EDIT: {}", video.caption))
                    .description(display_description)
                    .input_message_content(InputMessageContent::Text(InputMessageContentText::new("⚙️ Preparing your video...")))
                    .reply_markup(dummy_keyboard)
                );
                results.push(result);
            }
    } else {
        let videos: Vec<VideoData> = if q.query.is_empty() {
            sqlx::query_as("SELECT file_id, caption FROM videos LIMIT ? OFFSET ?")
                .bind(PAGE_SIZE).bind(sql_offset).fetch_all(&pool).await.unwrap_or_default()
        } else {
            let pattern = format!("%{}%", q.query);
            sqlx::query_as("SELECT file_id, caption FROM videos WHERE caption LIKE ? LIMIT ? OFFSET ?")
                .bind(pattern).bind(PAGE_SIZE).bind(sql_offset).fetch_all(&pool).await.unwrap_or_default()
        };

        results = videos.into_iter().enumerate().map(|(_, video)| {
            let mut result_id = video.file_id.clone();
            result_id.truncate(60);
            InlineQueryResult::CachedVideo(InlineQueryResultCachedVideo::new(result_id, video.file_id, video.caption.clone()))
        }).collect();
    }

    let next_offset = if results.len() == PAGE_SIZE as usize {
        Some((page + 1).to_string())
    } else {
        None
    };

    let mut answer = bot.answer_inline_query(q.id, results);
    if let Some(offset) = next_offset {
        answer = answer.next_offset(offset);
    }
    answer.await?;

    Ok(())
}

/// --- NEW: Background task to process and save a video ---
/// --- NEW: Background task to process and save a video ---
/// --- MODIFIED: Background task to process, CROP, and save a video ---
/// --- MODIFIED: Background task to process, CROP, and save a video ---
async fn process_and_save_video(
    bot: Bot,
    chat_id: ChatId,
    user_message_id: MessageId,
    status_message_id: MessageId,
    video: Video,
    caption: String,
    pool: SharedState,
) {
    let temp_dir = match Builder::new().prefix("video_save").tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            log::error!("Failed to create temp dir: {}", e);
            let _ = bot.edit_message_text(chat_id, status_message_id, "❌ Error: Server failed to create temporary directory.").await;
            return;
        }
    };
    let temp_dir_path = temp_dir.path();
    let input_path = temp_dir_path.join("input.mp4");
    let output_path = temp_dir_path.join("output.mp4");
    let frame_path = temp_dir_path.join("frame.png");

    let final_file_id: String;
    let final_message_text: String;

    'processing_block: {
        let Ok(file) = bot.get_file(&video.file.id).await else {
            let _ = bot.edit_message_text(chat_id, status_message_id, "❌ Error: Failed to get file info from Telegram.").await;
            return;
        };
        let Ok(mut dest) = fs::File::create(&input_path).await else { return; };
        if bot.download_file(&file.path, &mut dest).await.is_err() {
            let _ = bot.edit_message_text(chat_id, status_message_id, "❌ Error: Failed to download video.").await;
            return;
        };

        let frame_extraction_status = tokio::process::Command::new("ffmpeg")
            .arg("-i").arg(&input_path).arg("-vframes").arg("1").arg(&frame_path).status().await.ok();
        if frame_extraction_status.is_none() || !frame_extraction_status.unwrap().success() {
            log::warn!("Failed to extract frame for video save check. Saving original.");
            final_file_id = video.file.id;
            final_message_text = "⚠️ Could not analyze video, saved original.".to_string();
            break 'processing_block;
        }

        let detected_boxes = detect_white_or_black_boxes(&frame_path);

        if detected_boxes.len() == 1 {
            let bbox = &detected_boxes[0];
            let video_height = video.height;

            // --- START OF FIX: Add a small margin to remove border pixels ---
            const CROP_MARGIN: u32 = 2; // Shave off 2 extra pixels to be safe.

            let is_top_box = (bbox.y as u32 + bbox.h / 2) < (video_height / 2);

            let filter_complex = if is_top_box {
                // The box is at the top. We want to keep the area *below* it.
                // Start the crop a few pixels below the box to ensure we remove any border line.
                let crop_start_y = (bbox.y as u32 + bbox.h + CROP_MARGIN).min(video_height);
                // The new video's height is the original height minus where the crop starts.
                let new_height = video_height - crop_start_y;

                format!(
                    "[0:v]crop=w=in_w:h={h}:x=0:y={y}[v_out]",
                    h = new_height,
                    y = crop_start_y
                )
            } else {
                // The box is at the bottom. We want to keep the area *above* it.
                // The new video's height is the y-coordinate where the box begins, minus a small margin.
                let new_height = (bbox.y as u32).saturating_sub(CROP_MARGIN);

                format!(
                    "[0:v]crop=w=in_w:h={h}:x=0:y=0[v_out]",
                    h = new_height
                )
            };
            // --- END OF FIX ---

            let mut command = tokio::process::Command::new("ffmpeg");
            command.arg("-i").arg(&input_path)
                   .arg("-filter_complex").arg(&filter_complex)
                   .arg("-map").arg("[v_out]")
                   .arg("-map").arg("0:a?")
                   .arg("-c:a").arg("copy");

            let encoder = env::var("FFMPEG_ENCODER").unwrap_or_default();
            if !encoder.is_empty() { command.arg("-c:v").arg(&encoder); }
            else if env::var("CUDA_ENABLED").is_ok() { command.arg("-c:v").arg("h264_nvenc").arg("-preset").arg("p7").arg("-rc").arg("vbr").arg("-gpu").arg("0"); }
            else { command.arg("-c:v").arg("libx264").arg("-preset").arg("ultrafast"); }

            command.arg("-flags").arg("+global_header").arg("-movflags").arg("+faststart").arg("-pix_fmt").arg("yuv420p").arg(&output_path);

            if command.status().await.is_ok_and(|s| s.success()) {
                let sent_message = match bot.send_video(chat_id, InputFile::file(&output_path)).caption(&caption).reply_to_message_id(user_message_id).await {
                    Ok(msg) => msg,
                    Err(e) => {
                        log::error!("Failed to send processed video: {}", e);
                        let _ = bot.edit_message_text(chat_id, status_message_id, "❌ Error: Could not send the processed video.").await;
                        return;
                    }
                };

                if let Some(new_video) = sent_message.video() {
                    final_file_id = new_video.file.id.clone();
                    final_message_text = "✅ Video cropped and saved!".to_string();
                } else {
                    let _ = bot.edit_message_text(chat_id, status_message_id, "❌ Error: Telegram did not return video data after upload.").await;
                    return;
                }
            } else {
                final_file_id = video.file.id;
                final_message_text = "⚠️ Video processing failed, saved original.".to_string();
            }
        } else {
            final_file_id = video.file.id;
            final_message_text = if detected_boxes.is_empty() {
                "✅ Video saved! (No removable box was detected)".to_string()
            } else {
                format!("✅ Video saved! ({} boxes detected, expected 1 for cropping)", detected_boxes.len())
            };
        }
    }

    if sqlx::query("INSERT OR IGNORE INTO videos (file_id, caption) VALUES (?, ?)")
        .bind(&final_file_id).bind(&caption).execute(&pool).await.is_ok()
    {
        bot.edit_message_text(chat_id, status_message_id, final_message_text).await.ok();
    } else {
        bot.edit_message_text(chat_id, status_message_id, "❌ DB error while saving video.").await.ok();
    }
}

/// --- MODIFIED: Spawns a background task for video processing ---
async fn handle_message(bot: Bot, msg: Message, pool: SharedState) -> Result<(), teloxide::RequestError> {
    let mut video_to_save: Option<&Video> = None;
    let mut caption_to_save: Option<&str> = None;

    if let (Some(video), Some(caption)) = (msg.video(), msg.caption()) {
        video_to_save = Some(video);
        caption_to_save = Some(caption);
    } else if let (Some(reply), Some(caption)) = (msg.reply_to_message(), msg.text()) {
        if let Some(video) = reply.video() {
            video_to_save = Some(video);
            caption_to_save = Some(caption);
        }
    }

    if let (Some(video), Some(caption)) = (video_to_save, caption_to_save) {
        let status_msg = bot.send_message(msg.chat.id, "⏳ Analyzing and saving video...").reply_to_message_id(msg.id).await?;

        tokio::spawn(process_and_save_video(
            bot.clone(),
            msg.chat.id,
            msg.id,
            status_msg.id,
            video.clone(),
            caption.to_string(),
            pool,
        ));
    } else {
        bot.send_message(msg.chat.id, "Send a video with a caption or reply to a video with a new caption to save it.").await?;
    }
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

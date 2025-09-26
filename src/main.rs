use teloxide::{prelude::*, types::*, utils::command::BotCommands};
use sqlx::SqlitePool;
use tempfile::Builder;
use tokio::fs;
use teloxide::net::Download;
use std::path::{Path, PathBuf};
use std::cmp::Reverse;
use std::env;

// Imports for computer vision and inline editing.
use image::io::Reader as ImageReader;
use imageproc::{contours::{find_contours, Contour}, rect::Rect};
use reqwest::Url;

// --- Data Structures ---

#[derive(Clone, Debug, sqlx::FromRow)]
struct VideoData { caption: String, file_id: String }
type SharedState = SqlitePool;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "These commands are supported:")]
enum Command {
    #[command(description = "Displays this help message")]
    Help,
    #[command(description = "Start a dialog to remove a saved video")]
    Remove,
}

// --- Computer Vision Logic ---
#[derive(Debug, Clone, Copy)]
struct BoundingBox { x: i32, y: i32, w: u32, h: u32 }

/// Helper function to convert contours into bounding boxes, filtering for a minimum size.
fn contours_to_bounding_boxes(
    contours: &[Contour<i32>],
    min_width: u32,
    min_height: u32,
) -> Vec<Rect> {
    contours.iter()
        .filter(|c| !c.points.is_empty())
        .map(|contour| {
            let mut min_x = i32::MAX; let mut min_y = i32::MAX;
            let mut max_x = i32::MIN; let mut max_y = i32::MIN;
            for point in &contour.points {
                min_x = min_x.min(point.x);
                min_y = min_y.min(point.y);
                max_x = max_x.max(point.x);
                max_y = max_y.max(point.y);
            }
            Rect::at(min_x, min_y).of_size((max_x - min_x + 1) as u32, (max_y - min_y + 1) as u32)
        })
        .filter(|rect| rect.width() >= min_width && rect.height() >= min_height)
        .collect()
}

/// Detects large boxes by first padding the image to reliably find shapes touching the edges.
fn detect_white_or_black_boxes(image_path: &Path) -> Vec<BoundingBox> {
    let Some(img) = ImageReader::open(image_path).ok().and_then(|r| r.decode().ok()) else { return vec![]; };
    let original_luma = img.to_luma8();
    let (original_width, original_height) = original_luma.dimensions();

    const PADDING: u32 = 1;
    let mut padded_image = image::GrayImage::new(original_width + PADDING * 2, original_height + PADDING * 2);
    image::imageops::replace(&mut padded_image, &original_luma, PADDING as i64, PADDING as i64);

    const MIN_BOX_WIDTH_RATIO: f32 = 0.4;
    const MIN_BOX_HEIGHT_RATIO: f32 = 0.1;
    let min_width = (original_width as f32 * MIN_BOX_WIDTH_RATIO) as u32;
    let min_height = (original_height as f32 * MIN_BOX_HEIGHT_RATIO) as u32;

    let white_binary_image = imageproc::map::map_pixels(&padded_image, |_, _, p| {
        if p[0] > 220 { image::Luma([255]) } else { image::Luma([0]) }
    });
    let contours = find_contours(&white_binary_image);
    let mut boxes = contours_to_bounding_boxes(&contours, min_width, min_height);

    log::info!("Detected {} white boxes.", boxes.len());
    if boxes.is_empty() {
        log::info!("No white boxes found, trying black boxes.");
        let black_binary_image = imageproc::map::map_pixels(&original_luma, |_, _, p| {
            if p[0] < 3 { image::Luma([255]) } else { image::Luma([0]) }
        });
        let black_contours = find_contours(&black_binary_image);
        boxes = contours_to_bounding_boxes(&black_contours, min_width, min_height);
    }
    log::info!("Detected {} boxes before filtering.", boxes.len());
    log::info!("Boxes: {:?}", boxes);
    boxes.sort_by_key(|b| Reverse(b.width() * b.height()));
    boxes.into_iter()
         .filter(|rect| rect.height() < original_height)
        .take(2)
        .map(|rect| BoundingBox {
            x: rect.left(),
            y: rect.top(),
            w: rect.width(),
            h: rect.height(),
        })
        .collect()
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
    sqlx::query(r#"CREATE TABLE IF NOT EXISTS videos (file_id TEXT PRIMARY KEY NOT NULL, caption TEXT NOT NULL, user_id INTEGER NOT NULL)"#)
        .execute(&pool).await.expect("Failed to create database table");

    let handler = dptree::entry()
        .branch(Update::filter_message().filter_command::<Command>().endpoint(handle_command))
        .branch(Update::filter_chosen_inline_result().endpoint(handle_chosen_inline_result))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query))
        .branch(Update::filter_message().endpoint(handle_message));

    Dispatcher::builder(bot, handler).dependencies(dptree::deps![pool]).enable_ctrlc_handler().build().dispatch().await;
}

/// Helper function to format seconds into H:MM:SS.cs for ASS subtitles.
fn format_ass_time(seconds: f64) -> String {
    let hours = (seconds / 3600.0).floor();
    let minutes = ((seconds % 3600.0) / 60.0).floor();
    let secs = (seconds % 60.0).floor();
    let centiseconds = ((seconds.fract() * 100.0).round() as i32).min(99);
    format!("{}:{:02}:{:02}.{:02}", hours, minutes, secs, centiseconds)
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
    let font_path_str = std::env::var("UNIVERSAL_FONT_PATH").expect("UNIVERSAL_FONT_PATH must be set in .env");
    let font_path = PathBuf::from(&font_path_str);
    let font_name = font_path.file_stem().and_then(|s| s.to_str()).unwrap_or("Noto Sans");

    let ass_content: String;
    let mut preliminary_filters: Vec<String> = vec![];
    let mut final_map_tag = "[0:v]".to_string();

    let is_timed_edit = messages.len() == 3 && messages[1].parse::<f64>().is_ok();

    if is_timed_edit {
        let text1 = messages[0].trim();
        let time_s = messages[1].parse::<f64>().unwrap_or(0.0);
        let text2 = messages[2].trim();

        let end_time1_str = format_ass_time(time_s);
        let start_time2_str = format_ass_time(time_s);
        let ass_safe_text1 = text1.replace('{', "\\{").replace('}', "\\}");
        let ass_safe_text2 = text2.replace('{', "\\{").replace('}', "\\}");

        if let Some(bbox) = detected_boxes.get(0) {
            // Case 1: Timed edit inside a detected box.
            // **FIX**: Add the drawbox filter to fill the box with white first.
            let current_tag = "[v_box]".to_string();
            let filter = format!(
                "{last_tag}drawbox=x={x}:y={y}:w={w}:h={h}:color=white:t=fill{out}",
                last_tag = &final_map_tag, // Starts as "[0:v]"
                x = bbox.x, y = bbox.y, w = bbox.w, h = bbox.h, out = &current_tag
            );
            preliminary_filters.push(filter);
            final_map_tag = current_tag; // The next filter will use the output of drawbox.

            let font_size = (bbox.h as f32 * 0.3).max(20.0) as u32;
            let margin_l = bbox.x;
            let margin_r = width as i32 - (bbox.x + bbox.w as i32);
            let margin_v = bbox.y + (bbox.h as f32 * 0.1) as i32;

            let event1 = format!(
                r#"Dialogue: 0,0:00:00.00,{end_time},BoxStyle,,{ml},{mr},{mv},,{{\fs{fs}}}{text}"#,
                end_time = end_time1_str, ml = margin_l, mr = margin_r, mv = margin_v, fs = font_size, text = ass_safe_text1
            );
            let event2 = format!(
                r#"Dialogue: 0,{start_time},9:59:59.99,BoxStyle,,{ml},{mr},{mv},,{{\fs{fs}}}{text}"#,
                start_time = start_time2_str, ml = margin_l, mr = margin_r, mv = margin_v, fs = font_size, text = ass_safe_text2
            );

            ass_content = format!(
                r#"[Script Info]
PlayResX: {width}
PlayResY: {height}
[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: BoxStyle,{font_name},100,&H00000000,&H000000FF,&H00FFFFFF,&H00FFFFFF,0,0,0,0,100,100,0,0,1,0,0,8,10,10,10,1
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
{event1}
{event2}"#,
                width = width, height = height, font_name = font_name, event1 = event1, event2 = event2
            );
        } else {
            // Case 2: Timed edit, but no box detected. Fallback to padding.
            let pad_height = (height as f32 * 0.15).max(100.0) as u32;
            let font_size = (pad_height as f32 * 0.4).max(30.0) as u32;
            let v_margin = (pad_height as f32 * 0.25) as u32;
            let padded_tag = "[padded_v]".to_string();
            preliminary_filters.push(format!("[0:v]pad=width=in_w:height=in_h+{pad}:x=0:y={pad}:color=black{out}", pad = pad_height, out = &padded_tag));
            final_map_tag = padded_tag;

            ass_content = format!(
                r#"[Script Info]
PlayResX: {width}
PlayResY: {height}
[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Caption,{font_name},{font_size},&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,1,8,10,10,{v_margin},1
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,{end_time1},Caption,,0,0,0,,{text1}
Dialogue: 0,{start_time2},9:59:59.99,Caption,,0,0,0,,{text2}"#,
                width = width, height = height + pad_height, font_name = font_name, font_size = font_size, v_margin = v_margin,
                end_time1 = end_time1_str, start_time2 = start_time2_str, text1 = ass_safe_text1, text2 = ass_safe_text2
            );
        }
    } else if detected_boxes.is_empty() {
        // Case 3: Non-timed edit, no boxes found. Pad the video.
        let full_text = messages.join("\\N").trim().to_string();
        if full_text.is_empty() {
             bot.edit_message_text_inline(&inline_message_id, "❌ Error: No text provided to add to video.").await.ok();
             return;
        }
        let pad_height = (height as f32 * 0.15).max(100.0) as u32;
        let font_size = (pad_height as f32 * 0.4).max(30.0) as u32;
        let v_margin = (pad_height as f32 * 0.25) as u32;
        let padded_tag = "[padded_v]".to_string();
        preliminary_filters.push(format!("[0:v]pad=width=in_w:height=in_h+{pad}:x=0:y={pad}:color=black{out}", pad = pad_height, out = &padded_tag));
        final_map_tag = padded_tag;

        ass_content = format!(
            r#"[Script Info]
PlayResX: {width}
PlayResY: {height}
[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Caption,{font_name},{font_size},&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,1,8,10,10,{v_margin},1
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,9:59:59.99,Caption,,0,0,0,,{text}"#,
            width = width, height = height + pad_height, font_name = font_name, font_size = font_size,
            v_margin = v_margin, text = full_text.replace('{', "\\{").replace('}', "\\}")
        );
    } else {
        // Case 4: Non-timed edit, boxes found. Fill boxes with text.
        let mut last_tag = final_map_tag; // Starts as "[0:v]"
        let mut event_lines = String::new();
        for (i, bbox) in detected_boxes.iter().take(messages.len()).enumerate() {
            let current_tag = format!("[v{}]", i);
            let filter = format!(
                "{last_tag}drawbox=x={x}:y={y}:w={w}:h={h}:color=white:t=fill{out}",
                last_tag = &last_tag, x = bbox.x, y = bbox.y, w = bbox.w, h = bbox.h, out = &current_tag
            );
            preliminary_filters.push(filter);
            last_tag = current_tag;

            let text_to_draw = messages.get(i).unwrap_or(&messages[0]).trim();
            let font_size = (bbox.h as f32 * 0.15).max(11.0) as u32;
            let ass_safe_text = text_to_draw.replace('{', "\\{").replace('}', "\\}");
            let margin_l = bbox.x;
            let margin_r = width as i32 - (bbox.x + bbox.w as i32);
            let margin_v = bbox.y + (bbox.h as f32 * 0.1) as i32;

            let event = format!(
                r#"Dialogue: 0,0:00:00.00,9:59:59.99,BoxStyle,,{ml},{mr},{mv},,{{\fs{fs}}}{text}"#,
                ml = margin_l, mr = margin_r, mv = margin_v, fs = font_size, text = ass_safe_text
            );
            event_lines.push_str(&event);
            event_lines.push('\n');
        }
        final_map_tag = last_tag;
        ass_content = format!(
            r#"[Script Info]
PlayResX: {width}
PlayResY: {height}
[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: BoxStyle,{font_name},100,&H00000000,&H000000FF,&H00FFFFFF,&H00FFFFFF,0,0,0,0,100,100,0,0,1,0,0,8,10,10,10,1
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
{event_lines}"#,
            width = width, height = height, font_name = font_name, event_lines = event_lines
        );
    }

    let ass_path = temp_dir_path.join("subs.ass");
    if tokio::fs::write(&ass_path, ass_content).await.is_err() {
        bot.edit_message_text_inline(&inline_message_id, "❌ Error: Could not write temporary subtitle file.").await.ok();
        return;
    }

    let escaped_ass_path = ass_path.to_string_lossy().replace('\\', "/");

    let final_filter_chain = if preliminary_filters.is_empty() {
         // This case should now only be hit if there are no boxes AND it's not a timed edit (which pads).
         // Adding it for robustness, but the logic flows should mean it's rarely, if ever, used.
        format!("[0:v]subtitles=filename='{subs_path}', format=yuv420p[v_out]", subs_path = escaped_ass_path)
    } else {
        let prelim_chain = preliminary_filters.join(";");
        format!("{prelim_chain}; {final_video_stream}subtitles=filename='{subs_path}', format=yuv420p[v_out]",
            prelim_chain = prelim_chain,
            final_video_stream = &final_map_tag,
            subs_path = escaped_ass_path)
    };

    let mut command = tokio::process::Command::new("ffmpeg");
    command.arg("-i").arg(&input_path).arg("-filter_complex").arg(&final_filter_chain)
        .arg("-map").arg("[v_out]").arg("-map").arg("0:a?").arg("-c:a").arg("copy");

    let encoder = env::var("FFMPEG_ENCODER").unwrap_or_default();
    if !encoder.is_empty() {
        command.arg("-c:v").arg(&encoder);
    } else if env::var("CUDA_ENABLED").is_ok() {
        command.arg("-c:v").arg("h264_nvenc").arg("-preset").arg("p7").arg("-rc").arg("vbr").arg("-gpu").arg("0");
    } else {
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
        let stderr = command.output().await.map(|o| String::from_utf8_lossy(&o.stderr).to_string()).unwrap_or_else(|e| e.to_string());
        log::error!("FFMPEG failed. Filter: '{}'. Stderr: {}", final_filter_chain, stderr);
        bot.edit_message_text_inline(&inline_message_id, "❌ An error occurred during video processing.").await.ok();
    }
}


// --- Bot Handlers ---

// Replace your existing handle_command function with this one.
async fn handle_command(bot: Bot, msg: Message, cmd: Command, pool: SharedState) -> Result<(), teloxide::RequestError> {
    // We need the user's ID for the remove command, so get it early.
    let Some(user) = msg.from() else {
        // Can't process command without a user.
        return Ok(());
    };
    let user_id = user.id;

    match cmd {
        Command::Help => {
            // Combine the auto-generated descriptions with detailed inline usage instructions.
            let command_descriptions = Command::descriptions().to_string();
            let help_text = format!(
                "{}\n\n*Inline Editing Guide*\n\n\
                To edit a video, type the bot's username in any chat, followed by a search for your saved video, then use `/edit`\n\n\
                *Examples:*\n\
                `@bot_username cat video /edit New funny text`\n\n\
                *Advanced Formats:*\n\n\
                *1 Two\\-Box Edit:*\n\
                Provide text for the top two detected boxes using `/box2`\n\
                `@bot_username /edit Top Text /box2 Bottom Text`\n\n\
                *2 Timed Text Edit:*\n\
                Change the text in the first detected box at a specific time\n\
                `@bot_username /edit Text Before /55 Text After`",
                command_descriptions
            );

            bot.send_message(msg.chat.id, help_text).parse_mode(ParseMode::MarkdownV2).await?;
        }
        Command::Remove => {
            // UPDATED QUERY: Select all columns and filter with a WHERE clause
            let videos: Vec<VideoData> = sqlx::query_as("SELECT file_id, caption, user_id FROM videos WHERE user_id = ?")
                .bind(user_id.0 as i64) // Bind the user's ID to the query
                .fetch_all(&pool)
                .await
                .unwrap_or_default();

            if videos.is_empty() {
                bot.send_message(msg.chat.id, "You have no saved videos to remove.").await?;
                return Ok(());
            }

            let keyboard_buttons: Vec<Vec<_>> = videos.into_iter().map(|video| {
                // The file_id from telegram can be very long, truncate for the callback data
                let mut short_id = video.file_id.clone();
                short_id.truncate(50);
                vec![InlineKeyboardButton::callback(video.caption, format!("delete_{}", short_id))]
            }).collect();

            bot.send_message(msg.chat.id, "Select one of your videos to remove:").reply_markup(InlineKeyboardMarkup::new(keyboard_buttons)).await?;
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
                let mut final_edit_text = String::new();

                // Parse for timed edit format first: `text1 /time text2`
                if let Some((msg1, rest)) = edit_params.rsplit_once('/') {
                    if let Some((time_str, msg2)) = rest.trim().split_once(' ') {
                        if time_str.parse::<f64>().is_ok() {
                            final_edit_text = format!("{}///{}///{}", msg1.trim(), time_str.trim(), msg2.trim());
                        }
                    }
                }

                // Fallback to other formats if timed edit was not parsed
                if final_edit_text.is_empty() {
                    if let Some((msg1, msg2)) = edit_params.split_once("/box2") {
                        final_edit_text = format!("{}///{}", msg1.trim(), msg2.trim());
                    } else {
                        final_edit_text = edit_params.to_string();
                    }
                }

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
        let user_id = q.from.id;
        // Check if we can actually send a message back to the user later.
        let can_send_message = bot.send_chat_action(user_id, ChatAction::Typing).await.is_ok();

        if can_send_message {
            // --- User has started the bot, proceed with edit logic ---
            let edit_params = edit_params_raw.trim();
            let mut display_description = String::new();

            if let Some((msg1, rest)) = edit_params.rsplit_once('/') {
                if let Some((time_str, msg2)) = rest.trim().split_once(' ') {
                    if let Ok(time) = time_str.parse::<f64>() {
                        display_description = format!("TEXT 1: '{}' | TEXT 2: '{}' (at {}s)", msg1.trim(), msg2.trim(), time);
                    }
                }
            }

            if display_description.is_empty() {
                if let Some((msg1, msg2)) = edit_params.split_once("/box2") {
                    display_description = format!("BOX 1: '{}' | BOX 2: '{}'", msg1.trim(), msg2.trim());
                } else {
                    display_description = format!("Click to replace text with: '{}'", edit_params);
                }
            }

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
            // --- User has NOT started the bot, show a prompt to start it ---
            let me = bot.get_me().await?;
            if let Some(bot_username) = &me.username {
                 let start_url_str = format!("https://t.me/{}?start=inline", bot_username);
                 if let Ok(start_url) = Url::parse(&start_url_str) {
                     let keyboard = InlineKeyboardMarkup::new(vec![vec![
                         InlineKeyboardButton::url("Click here to Start Bot", start_url)
                     ]]);

                     let result = InlineQueryResult::Article(
                         InlineQueryResultArticle::new(
                             "start_bot_prompt", // Unique ID for this result
                             "Bot Not Started",
                             InputMessageContent::Text(InputMessageContentText::new(
                                 "You need to start a chat with me before I can edit and send you videos."
                             ))
                         )
                         .description("You must start the bot to use the edit feature.")
                         .reply_markup(keyboard)
                     );
                     results.push(result.into());
                 }
            }
        }
    } else {
        // --- Standard search logic (no changes needed here) ---
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

    if q.query.contains("/edit") {
        answer = answer.cache_time(0);
    }

    answer.await?;

    Ok(())
}

async fn download_and_process_video(
    bot: Bot,
    chat_id: ChatId,
    user_message_id: MessageId,
    status_message_id: MessageId,
    url: String,
    caption: String,
    pool: SharedState,
    user_id: UserId,
) {
    let temp_dir = match Builder::new().prefix("video_dl").tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            log::error!("Failed to create temp dir: {}", e);
            let _ = bot.edit_message_text(chat_id, status_message_id, "❌ Error: Server failed to create temporary directory.").await;
            return;
        }
    };
    let temp_dir_path = temp_dir.path();
    let output_template = temp_dir_path.join("video.mp4");

    // Download video using yt-dlp
    let ytdlp_status = tokio::process::Command::new("yt-dlp")
        .arg("--output").arg(output_template)
        .arg("--force-overwrite")
        .arg("--format").arg("bv*[ext=mp4][filesize<10M]+ba[ext=m4a]/b[ext=mp4][filesize<10M]/bv*+ba/b")
        .arg("--cookies").arg("./instacookie")
        .arg("--remux-video").arg("mp4")
        .arg(&url)
        .status().await;

    if !ytdlp_status.is_ok_and(|s| s.success()) {
        log::error!("yt-dlp failed for url {}", &url);
        bot.edit_message_text(chat_id, status_message_id, "❌ Error: Download failed. The link may be invalid or private.").await.ok();
        return;
    }

    let input_path = temp_dir_path.join("video.mp4");
    if !input_path.exists() {
        bot.edit_message_text(chat_id, status_message_id, "❌ Error: Downloaded video file not found.").await.ok();
        return;
    }

    let ffprobe_output = match tokio::process::Command::new("ffprobe")
        .arg("-v").arg("error")
        .arg("-select_streams").arg("v:0")
        .arg("-show_entries").arg("stream=height")
        .arg("-of").arg("csv=p=0")
        .arg(&input_path)
        .output().await {
            Ok(out) => out,
            Err(e) => {
                log::error!("ffprobe failed: {}", e);
                bot.edit_message_text(chat_id, status_message_id, "❌ Error: Could not analyze downloaded video.").await.ok();
                return;
            }
        };

    let video_height: u32 = String::from_utf8(ffprobe_output.stdout).unwrap_or_default().trim().parse().unwrap_or(0);
    if video_height == 0 {
        bot.edit_message_text(chat_id, status_message_id, "❌ Error: Could not determine video height from download.").await.ok();
        return;
    }

    let output_path = temp_dir_path.join("output.mp4");
    let frame_path = temp_dir_path.join("frame.png");

    let final_file_id: String;
    let final_message_text: String;

    'processing_block: {
        let frame_extraction_status = tokio::process::Command::new("ffmpeg")
            .arg("-i").arg(&input_path).arg("-vframes").arg("1").arg(&frame_path).status().await.ok();

        if frame_extraction_status.is_none() || !frame_extraction_status.unwrap().success() {
            log::warn!("Failed to extract frame from downloaded video. Saving original.");
            let sent = bot.send_video(chat_id, InputFile::file(&input_path)).caption(&caption).reply_to_message_id(user_message_id).await;
            if let Ok(sent_message) = sent {
                if let Some(video) = sent_message.video() {
                    final_file_id = video.file.id.clone();
                    final_message_text = "⚠️ Could not analyze video, saved original.".to_string();
                } else {
                    bot.edit_message_text(chat_id, status_message_id, "❌ Error: Telegram did not return video data after upload.").await.ok(); return;
                }
            } else {
                bot.edit_message_text(chat_id, status_message_id, "❌ Error: Failed to upload processed video.").await.ok(); return;
            }
            break 'processing_block;
        }

        let detected_boxes = detect_white_or_black_boxes(&frame_path);

        if detected_boxes.len() == 1 {
            let bbox = &detected_boxes[0];
            const CROP_MARGIN: u32 = 2;
            let is_top_box = (bbox.y as u32 + bbox.h / 2) < (video_height / 2);
            let filter_complex = if is_top_box {
                let crop_start_y = (bbox.y as u32 + bbox.h + CROP_MARGIN).min(video_height);
                let new_height = video_height - crop_start_y;
                format!("[0:v]crop=w=in_w:h={h}:x=0:y={y}[v_out]", h = new_height, y = crop_start_y)
            } else {
                let new_height = (bbox.y as u32).saturating_sub(CROP_MARGIN);
                format!("[0:v]crop=w=in_w:h={h}:x=0:y=0[v_out]", h = new_height)
            };

            let mut command = tokio::process::Command::new("ffmpeg");
            command.arg("-i").arg(&input_path)
                   .arg("-filter_complex").arg(&filter_complex)
                   .arg("-map").arg("[v_out]").arg("-map").arg("0:a?").arg("-c:a").arg("copy");
            let encoder = env::var("FFMPEG_ENCODER").unwrap_or_default();
            if !encoder.is_empty() { command.arg("-c:v").arg(&encoder); }
            else if env::var("CUDA_ENABLED").is_ok() { command.arg("-c:v").arg("h264_nvenc").arg("-preset").arg("p7").arg("-rc").arg("vbr").arg("-gpu").arg("0"); }
            else { command.arg("-c:v").arg("libx264").arg("-preset").arg("ultrafast"); }
            command.arg("-flags").arg("+global_header").arg("-movflags").arg("+faststart").arg("-pix_fmt").arg("yuv420p").arg(&output_path);

            if command.status().await.is_ok_and(|s| s.success()) {
                let sent = bot.send_video(chat_id, InputFile::file(&output_path)).caption(&caption).reply_to_message_id(user_message_id).await;
                 if let Ok(sent_message) = sent {
                    if let Some(video) = sent_message.video() {
                        final_file_id = video.file.id.clone();
                        final_message_text = "✅ Video cropped and saved!".to_string();
                    } else {
                        bot.edit_message_text(chat_id, status_message_id, "❌ Error: Telegram did not return video data after upload.").await.ok(); return;
                    }
                } else {
                    bot.edit_message_text(chat_id, status_message_id, "❌ Error: Failed to upload processed video.").await.ok(); return;
                }
            } else {
                log::warn!("ffmpeg crop failed for downloaded video. Saving original.");
                let sent = bot.send_video(chat_id, InputFile::file(&input_path)).caption(&caption).reply_to_message_id(user_message_id).await;
                if let Ok(sent_message) = sent {
                    if let Some(video) = sent_message.video() {
                        final_file_id = video.file.id.clone();
                        final_message_text = "⚠️ Video processing failed, saved original.".to_string();
                    } else {
                        bot.edit_message_text(chat_id, status_message_id, "❌ Error: Telegram did not return video data after upload.").await.ok(); return;
                    }
                } else {
                    bot.edit_message_text(chat_id, status_message_id, "❌ Error: Failed to upload original video.").await.ok(); return;
                }
            }
        } else {
            let sent = bot.send_video(chat_id, InputFile::file(&input_path)).caption(&caption).reply_to_message_id(user_message_id).await;
            if let Ok(sent_message) = sent {
                if let Some(video) = sent_message.video() {
                    final_file_id = video.file.id.clone();
                    final_message_text = if detected_boxes.is_empty() { "✅ Video saved! (No removable box was detected)".to_string() } else { format!("✅ Video saved! ({} boxes detected, expected 1 for cropping)", detected_boxes.len()) };
                } else {
                    bot.edit_message_text(chat_id, status_message_id, "❌ Error: Telegram did not return video data after upload.").await.ok(); return;
                }
            } else {
                bot.edit_message_text(chat_id, status_message_id, "❌ Error: Failed to upload original video.").await.ok(); return;
            }
        }
    }

    let user_id_i64 = user_id.0 as i64;
    if sqlx::query("INSERT OR IGNORE INTO videos (file_id, caption, user_id) VALUES (?, ?, ?)")
        .bind(&final_file_id).bind(&caption).bind(user_id_i64).execute(&pool).await.is_ok()
    {
        bot.edit_message_text(chat_id, status_message_id, final_message_text).await.ok();
    } else {
        bot.edit_message_text(chat_id, status_message_id, "❌ DB error while saving video.").await.ok();
    }
}

async fn process_and_save_video(
    bot: Bot,
    chat_id: ChatId,
    user_message_id: MessageId,
    status_message_id: MessageId,
    video: Video,
    caption: String,
    pool: SharedState,
    user_id: UserId
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

            const CROP_MARGIN: u32 = 2;

            let is_top_box = (bbox.y as u32 + bbox.h / 2) < (video_height / 2);

            let filter_complex = if is_top_box {
                let crop_start_y = (bbox.y as u32 + bbox.h + CROP_MARGIN).min(video_height);
                let new_height = video_height - crop_start_y;

                log::info!("Cropping top box at y={} with height={}", crop_start_y, new_height);
                format!(
                    "[0:v]crop=w=in_w:h={h}:x=0:y={y}[v_out]",
                    h = new_height,
                    y = crop_start_y
                )
            } else {
                log::info!("Cropping bottom box at y={} with height={}", bbox.y, bbox.h);
                let new_height = (bbox.y as u32).saturating_sub(CROP_MARGIN);
                format!(
                    "[0:v]crop=w=in_w:h={h}:x=0:y=0[v_out]",
                    h = new_height
                )
            };

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

    let user_id_i64 = user_id.0 as i64;
    if sqlx::query("INSERT OR IGNORE INTO videos (file_id, caption, user_id) VALUES (?, ?, ?)")
        .bind(&final_file_id)
        .bind(&caption)
        .bind(user_id_i64) // <-- BIND the user_id
        .execute(&pool)
        .await
        .is_ok()
    {
        bot.edit_message_text(chat_id, status_message_id, final_message_text).await.ok();
    } else {
        bot.edit_message_text(chat_id, status_message_id, "❌ DB error while saving video.").await.ok();
    }
}

// Replace the existing handle_message function with this one
async fn handle_message(bot: Bot, msg: Message, pool: SharedState) -> Result<(), teloxide::RequestError> {
    let mut video_to_save: Option<&Video> = None;
    let mut caption_to_save: Option<&str> = None;
    let mut source_message_for_reply = &msg; // The message we should reply to

    if let (Some(video), Some(caption)) = (msg.video(), msg.caption()) {
        video_to_save = Some(video);
        caption_to_save = Some(caption);
    } else if let (Some(reply), Some(caption)) = (msg.reply_to_message(), msg.text()) {
        if let Some(video) = reply.video() {
            video_to_save = Some(video);
            caption_to_save = Some(caption);
            source_message_for_reply = reply; // Reply to the original video message
        }
    }

    // Ensure we have a user to attribute the save to
    let Some(user) = msg.from() else {
        // This can happen with anonymous messages in channels, ignore them.
        return Ok(());
    };

    if let (Some(video), Some(caption)) = (video_to_save, caption_to_save) {
        let status_msg = bot.send_message(msg.chat.id, "⏳ Analyzing and saving video...").reply_to_message_id(msg.id).await?;

        tokio::spawn(process_and_save_video(
            bot.clone(),
            msg.chat.id,
            source_message_for_reply.id, // Use the correct message to reply to
            status_msg.id,
            video.clone(),
            caption.to_string(),
            pool,
            user.id
        ));
    } else if let Some(text) = msg.text() {
        let maybe_url = text.split_whitespace().find(|s| {
            s.contains("douyin.com") || s.contains("vk.com") ||
            s.contains("youtube.com/clip/") || s.contains("youtube.com/shorts/") ||
            s.contains("instagram.com/reel/") || s.contains("bsky.app") ||
            s.contains("x.com/") || s.contains("twitter.com/") ||
            s.contains("reddit.com/") || s.contains("tiktok.com")
        });

        if let Some(url) = maybe_url {
            let caption = text.replace(url, "").trim().to_string();
            if caption.is_empty() {
                bot.send_message(msg.chat.id, "Please provide a caption for the video link.").await?;
                return Ok(());
            }

            let status_msg = bot.send_message(msg.chat.id, "⏳ Downloading and saving video...").reply_to_message_id(msg.id).await?;

            tokio::spawn(download_and_process_video(
                bot.clone(),
                msg.chat.id,
                msg.id,
                status_msg.id,
                url.to_string(),
                caption,
                pool,
                user.id,
            ));
        } else {
            bot.send_message(msg.chat.id, "Send a video with a caption or reply to a video with a new caption to save it.").await?;
        }
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

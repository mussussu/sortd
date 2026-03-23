use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClassificationResult {
    pub category: String,
    pub confidence: f64,
    pub suggested_folder: String,
    pub reasoning: String,
}

/// Classify by file extension only — no AI call, returns quickly.
/// Returns `None` if the extension is unrecognised.
pub fn fast_classify(path: &Path) -> Option<ClassificationResult> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())?;

    let (category, suggested_folder) = match ext.as_str() {
        // Images
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "ico" | "tiff" => {
            ("Images", "Images/Photos")
        }
        "svg" => ("Images", "Images/Vector"),
        // Design
        "psd" | "ai" | "figma" | "sketch" => ("Images", "Images/Design"),
        // Videos
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "flv" => ("Videos", "Videos"),
        // Audio
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" => ("Music", "Music"),
        // Documents
        "doc" | "docx" | "txt" | "rtf" | "odt" => ("Documents", "Documents"),
        // PDFs
        "pdf" => ("PDFs", "Documents/PDFs"),
        // Spreadsheets
        "xls" | "xlsx" | "csv" | "ods" => ("Spreadsheets", "Documents/Spreadsheets"),
        // Code
        "rs" | "js" | "ts" | "py" | "go" | "java" | "cpp" | "c" | "h" | "css" | "html"
        | "json" | "toml" | "yaml" => ("Code", "Code"),
        // Archives
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" => ("Archives", "Archives"),
        // Installers
        "exe" | "msi" | "dmg" | "deb" | "rpm" => ("Installers", "Installers"),
        _ => return None,
    };

    Some(ClassificationResult {
        category: category.to_string(),
        confidence: 0.95,
        suggested_folder: suggested_folder.to_string(),
        reasoning: format!("Classified by file extension '.{ext}'"),
    })
}

// ── Ollama API types ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
    format: String,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

/// Read up to `max_bytes` from the beginning of a file, returning a
/// lossy-UTF8 string (safe for embedding in a prompt).
fn read_file_preview(path: &Path, max_bytes: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let slice = &bytes[..bytes.len().min(max_bytes)];
    String::from_utf8_lossy(slice).into_owned()
}

/// Call the Ollama API and classify the file with llama3.2.
/// Times out after 30 seconds.
/// Falls back to extension-based classification if Ollama is unreachable.
pub async fn ai_classify(path: &Path) -> Result<ClassificationResult, String> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let preview = read_file_preview(path, 500);
    let preview_section = if preview.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nFile preview (first 500 bytes):\n{preview}")
    };

    let prompt = format!(
        r#"You are a file organiser. Classify the following file and respond with valid JSON only — no markdown, no explanation outside the JSON object.

File name: {file_name}{preview_section}

Respond with this exact JSON structure:
{{
  "category": "<one of: Documents, Images, Videos, Music, Code, Archives, Spreadsheets, PDFs, Installers, Other>",
  "confidence": <float 0.0–1.0>,
  "suggested_folder": "<relative path, e.g. Documents/Invoices>",
  "reasoning": "<one sentence>"
}}"#
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let request_body = OllamaRequest {
        model: "llama3.2".to_string(),
        prompt,
        stream: false,
        format: "json".to_string(),
    };

    let response = client
        .post("http://localhost:11434/api/generate")
        .json(&request_body)
        .send()
        .await;

    let response = match response {
        Ok(r) => r,
        Err(_) => {
            // Ollama unreachable — fall back to extension classification.
            return fast_classify(path).ok_or_else(|| {
                "Ollama unreachable and extension is unrecognised".to_string()
            });
        }
    };

    if !response.status().is_success() {
        // Non-2xx from Ollama — fall back.
        return fast_classify(path)
            .ok_or_else(|| format!("Ollama returned {}", response.status()));
    }

    let ollama_resp = response
        .json::<OllamaResponse>()
        .await
        .map_err(|e| format!("Failed to parse Ollama envelope: {e}"))?;

    let result: ClassificationResult = serde_json::from_str(&ollama_resp.response)
        .map_err(|e| format!("Failed to parse classification JSON: {e}"))?;

    Ok(result)
}

/// Main entry point.  Tries the fast (extension-only) path first; if
/// confidence is high enough returns immediately without touching the network.
/// Otherwise delegates to the AI classifier.
pub async fn classify(path: &Path) -> Result<ClassificationResult, String> {
    if let Some(result) = fast_classify(path) {
        if result.confidence > 0.9 {
            return Ok(result);
        }
    }

    ai_classify(path).await
}

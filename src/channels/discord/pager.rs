use serde_json::{Value, json};

use crate::channels::discord::command_dispatch::Ctx;

pub const PAGE_LIMIT: usize = 3750;
pub const TTL_SECS: u64 = 86_400;
const KIND: &str = "pager";

fn close_page(pages: &mut Vec<String>, cur: &mut String, fence: &Option<String>) {
    let mut page = std::mem::take(cur);
    if let Some(lang) = fence {
        page.push_str("\n```");
        *cur = format!("```{lang}");
    }
    pages.push(page);
}

pub fn paginate(text: &str, limit: usize) -> Vec<String> {
    let mut pages: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut fence: Option<String> = None;
    for line in text.split('\n') {
        if line.len() > limit {
            if !cur.is_empty() {
                close_page(&mut pages, &mut cur, &fence);
                cur.clear();
            }
            let mut rest = line;
            while rest.len() > limit {
                let cut = rest
                    .char_indices()
                    .take_while(|(i, _)| *i <= limit)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(limit);
                let chunk = match &fence {
                    Some(lang) => format!("```{lang}\n{}\n```", &rest[..cut]),
                    None => rest[..cut].to_string(),
                };
                pages.push(chunk);
                rest = &rest[cut..];
            }
            cur = match &fence {
                Some(lang) => format!("```{lang}\n{rest}"),
                None => rest.to_string(),
            };
        } else {
            if !cur.is_empty() && cur.len() + 1 + line.len() > limit {
                close_page(&mut pages, &mut cur, &fence);
            }
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(line);
        }
        let t = line.trim_start();
        if let Some(marker) = t.strip_prefix("```") {
            fence = if fence.is_none() {
                Some(marker.trim().to_string())
            } else {
                None
            };
        }
    }
    if !cur.is_empty() || pages.is_empty() {
        pages.push(cur);
    }
    pages
}

pub fn nav_row(token: &str, cur: usize, total: usize) -> Option<Value> {
    if total < 2 {
        return None;
    }
    Some(json!({"type": 1, "components": [
        {"type": 2, "style": 2, "custom_id": format!("pg:{token}:{}", cur.saturating_sub(1)), "label": "◀ Prev", "disabled": cur == 0},
        {"type": 2, "style": 2, "custom_id": format!("pg:{token}:{cur}"), "label": format!("Page {}/{total}", cur + 1), "disabled": true},
        {"type": 2, "style": 2, "custom_id": format!("pg:{token}:{}", cur + 1), "label": "Next ▶", "disabled": cur + 1 >= total},
    ]}))
}

pub fn container(text: &str, title: &str, accent: u32) -> Value {
    let mut inner: Vec<Value> = Vec::new();
    if !title.trim().is_empty() {
        inner.push(json!({"type": 10, "content": format!("### {}", crate::channels::discord::text::sanitize(title))}));
    }
    inner.push(json!({"type": 10, "content": text}));
    json!({"type": 17, "accent_color": accent, "components": inner})
}

pub fn components(
    pages: &[String],
    cur: usize,
    token: &str,
    title: &str,
    accent: u32,
) -> Vec<Value> {
    let text = pages
        .get(cur)
        .cloned()
        .unwrap_or_else(|| "(empty)".to_string());
    let mut out = vec![container(&text, title, accent)];
    if let Some(row) = nav_row(token, cur, pages.len()) {
        out.push(row);
    }
    out
}

pub fn mint(
    store: &crate::channels::discord::durable::Store,
    user: &str,
    channel: &str,
    pages: &[String],
    title: &str,
    accent: u32,
) -> Result<String, String> {
    let body = state_body(pages, title, accent);
    store.component_state_create(KIND, user, channel, &body, TTL_SECS)
}

pub fn state_body(pages: &[String], title: &str, accent: u32) -> String {
    json!({"pages": pages, "title": title, "accent": accent}).to_string()
}

pub async fn press(ctx: &Ctx<'_>, custom_id: &str, message: Option<&Value>) {
    let Some((token, idx)) = custom_id
        .strip_prefix("pg:")
        .and_then(|rest| rest.rsplit_once(':'))
        .and_then(|(t, i)| i.parse::<usize>().ok().map(|i| (t.to_string(), i)))
    else {
        return;
    };
    let state = ctx
        .store
        .component_state_get(&token, ctx.user_id, ctx.channel_id, KIND);
    let body = match state {
        Ok(Some(resource)) => serde_json::from_str::<Value>(&resource).unwrap_or_default(),
        Ok(None) => {
            crate::channels::discord::command_dispatch::expired(
                ctx,
                "Pager",
                "That pager expired.",
            )
            .await;
            return;
        }
        Err(e) => {
            crate::channels::discord::eprintln_stamped!("[discord] pager state failed: {e}");
            return;
        }
    };
    let pages: Vec<String> = body
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if pages.is_empty() {
        crate::channels::discord::command_dispatch::expired(ctx, "Pager", "That pager expired.")
            .await;
        return;
    }
    let cur = idx.min(pages.len() - 1);
    let title = body.get("title").and_then(Value::as_str).unwrap_or("");
    let accent = body
        .get("accent")
        .and_then(Value::as_u64)
        .unwrap_or(crate::channels::discord::render::INFO_ACCENT as u64) as u32;
    let mut comps = vec![container(
        pages.get(cur).cloned().unwrap_or_default().as_str(),
        title,
        accent,
    )];
    let trailing: Vec<Value> = message
        .and_then(Value::as_array)
        .map(|all| {
            all.iter()
                .skip(1)
                .filter(|c| !is_pager_nav(c))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    comps.extend(trailing);
    if let Some(row) = nav_row(&token, cur, pages.len()) {
        comps.push(row);
    }
    if let Err(e) = ctx
        .rest
        .interaction_update_v2(ctx.int_id, ctx.int_token, &comps)
        .await
    {
        crate::channels::discord::eprintln_stamped!("[discord] pager update failed: {e}");
        let _ = ctx.rest.defer_update(ctx.int_id, ctx.int_token).await;
    }
}

fn is_pager_nav(component: &Value) -> bool {
    component.get("type").and_then(Value::as_u64) == Some(1)
        && component
            .get("components")
            .and_then(Value::as_array)
            .map(|children| {
                children.iter().any(|child| {
                    child
                        .get("custom_id")
                        .and_then(Value::as_str)
                        .map(|id| id.starts_with("pg:"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
}

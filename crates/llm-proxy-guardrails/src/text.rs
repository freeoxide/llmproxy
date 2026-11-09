//! Walking the text of core requests and responses, and masking it in
//! place. Text lives in `CoreContent::Text` blocks (system, message
//! content, response content); opaque provider sources are not text and
//! are never touched.

use llm_proxy_protocol::core::{CoreContent, CoreRequest, CoreResponse};

/// Every text block of a request: system prompt texts then message
/// content texts, in order.
pub fn request_texts(request: &CoreRequest) -> impl Iterator<Item = String> {
    request
        .system
        .iter()
        .chain(
            request
                .messages
                .iter()
                .flat_map(|message| message.content.iter()),
        )
        .filter_map(text_of)
}

/// Every text block of a completed response.
pub fn response_texts(response: &CoreResponse) -> impl Iterator<Item = String> {
    response.content.iter().filter_map(text_of)
}

fn text_of(content: &CoreContent) -> Option<String> {
    match content {
        CoreContent::Text { text, .. } => Some(text.clone()),
        _ => None,
    }
}

/// Rewrites every request text in place for which `mask` returns a
/// replacement. `mask` receives the text by value (rules may need to
/// rebuild it) and its answer replaces the block verbatim.
pub async fn mask_request_texts<F, Fut>(request: &mut CoreRequest, mut mask: F)
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    for content in request.system.iter_mut() {
        if let CoreContent::Text { text, .. } = content {
            rewrite(text, &mut mask).await;
        }
    }
    for message in request.messages.iter_mut() {
        for content in message.content.iter_mut() {
            if let CoreContent::Text { text, .. } = content {
                rewrite(text, &mut mask).await;
            }
        }
    }
}

/// Rewrites every response content text in place for which `mask`
/// returns a replacement.
pub async fn mask_response_texts<F, Fut>(response: &mut CoreResponse, mut mask: F)
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    for content in response.content.iter_mut() {
        if let CoreContent::Text { text, .. } = content {
            rewrite(text, &mut mask).await;
        }
    }
}

async fn rewrite<F, Fut>(text: &mut String, mask: &mut F)
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    // The rule receives the text by value (it may rebuild it), but a `None`
    // answer means "leave this block alone" — the original must survive,
    // not be emptied by the hand-off.
    let offered = text.clone();
    if let Some(redacted) = mask(offered).await {
        *text = redacted;
    }
}

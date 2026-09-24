const CRAWLER_MARKERS: &[&str] = &[
    "bot",
    "crawl",
    "spider",
    "scrap",
    "slurp",
    "archiver",
    "anthropic-ai",
    "claude-web",
    "claude-user",
    "chatgpt-user",
    "cohere-ai",
    "perplexity",
    "mistralai-user",
    "meta-externalagent",
    "meta-externalfetcher",
    "googleother",
    "omgili",
    "img2dataset",
];

// Chat apps fetching a single link to render its preview, not crawling.
const LINK_PREVIEWERS: &[&str] = &[
    "slackbot",
    "slack-imgproxy",
    "discordbot",
    "twitterbot",
    "telegrambot",
    "whatsapp",
    "facebookexternalhit",
    "linkedinbot",
    "mastodon",
    "skypeuripreview",
];

pub fn is_crawler(user_agent: &str) -> bool {
    let user_agent = user_agent.to_ascii_lowercase();
    let mentions = |markers: &[&str]| markers.iter().any(|m| user_agent.contains(m));
    // Cubot is an Android phone brand.
    mentions(CRAWLER_MARKERS) && !mentions(LINK_PREVIEWERS) && !user_agent.contains("cubot")
}

pub fn robots_txt() -> String {
    let mut robots: String = LINK_PREVIEWERS
        .iter()
        .map(|agent| format!("User-agent: {agent}\n"))
        .collect();
    robots.push_str(
        "Allow: /\n\n\
         User-agent: *\n\
         Content-Signal: search=no, ai-input=no, ai-train=no\n\
         Disallow: /\n",
    );
    robots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crawlers_are_blocked() {
        for agent in [
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.2; +https://openai.com/gptbot)",
            "Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)",
            "CCBot/2.0 (https://commoncrawl.org/faq/)",
            "Mozilla/5.0 (Linux; Android 5.0) AppleWebKit/537.36 (KHTML, like Gecko) Mobile Safari/537.36 (compatible; Bytespider; spider-feedback@bytedance.com)",
            "meta-externalagent/1.1 (+https://developers.facebook.com/docs/sharing/webmasters/crawler)",
            "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; ChatGPT-User/1.0; +https://openai.com/bot)",
            "Scrapy/2.11 (+https://scrapy.org)",
        ] {
            assert!(is_crawler(agent), "{agent}");
        }
    }

    #[test]
    fn people_and_link_previews_are_allowed() {
        for agent in [
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
            "Mozilla/5.0 (Linux; Android 10; CUBOT X30) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36",
            "curl/8.7.1",
            "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)",
            "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
            "facebookexternalhit/1.1 Facebot Twitterbot/1.0",
            "TelegramBot (like TwitterBot)",
        ] {
            assert!(!is_crawler(agent), "{agent}");
        }
    }
}

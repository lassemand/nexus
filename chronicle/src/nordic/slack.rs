//! Slack notifications for periodic reports (NEX-221): the message format, the
//! exactly-once claim table and a small `chat.postMessage` client.

use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use sqlx::{PgPool, Row};
use thiserror::Error;
use tracing::{error, warn};
use uuid::Uuid;

use super::classify::ReportKind;

const STALE_MINUTES: i32 = 10;
const MAX_ATTEMPTS: i32 = 5;
const MAX_ERROR_CHARS: usize = 500;
const MAX_SEND_ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER_SECS: u64 = 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// Everything the message shows about one released report.
#[derive(Debug, Clone)]
pub struct ReportNotice {
    pub company_name: String,
    pub ticker: String,
    pub exchange_mic: String,
    pub report_kind: ReportKind,
    /// `2026-Q3` or `2026`, as produced by the classifier.
    pub fiscal_period: Option<String>,
    pub published_at: DateTime<Utc>,
    pub time_zone: Tz,
    /// `pre-open`, `intraday`, `after close` or `non-trading day`.
    pub session: Option<String>,
    pub headline: String,
    pub mfn_url: Option<String>,
    pub pdf_url: Option<String>,
    /// Posted by the daily poll because the live listener missed it.
    pub late: bool,
    /// MFN item this notice was built from (stored on the claim row).
    pub news_id: Uuid,
    pub lang: Option<String>,
}

impl ReportNotice {
    /// Twin-stable identity: same company, instant and report kind.
    pub fn report_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.ticker,
            self.exchange_mic,
            self.published_at.timestamp(),
            self.report_kind.as_str()
        )
    }
}

/// A message ready for Slack: mrkdwn `text`, which also serves as the
/// notification fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackMessage {
    pub text: String,
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn period_label(kind: ReportKind, period: Option<&str>) -> String {
    let kind_word = kind.as_str();
    let Some(p) = period.map(str::trim).filter(|p| !p.is_empty()) else {
        return kind_word.to_string();
    };
    match p.split_once('-') {
        Some((year, quarter)) => format!("{quarter} {year} {kind_word}"),
        None => format!("{p} {kind_word}"),
    }
}

/// Formats the Slack message for a report. Pure.
pub fn format_report_message(r: &ReportNotice) -> SlackMessage {
    let local = r.published_at.with_timezone(&r.time_zone);
    let mut when = local.format("%-d %b %Y %H:%M %Z").to_string();
    if let Some(s) = r.session.as_deref().filter(|s| !s.is_empty()) {
        when.push_str(" · ");
        when.push_str(&escape(s));
    }

    let mut lines = vec![
        format!(
            "📊 *{} ({})* · {}",
            escape(&r.company_name),
            escape(&r.ticker),
            period_label(r.report_kind, r.fiscal_period.as_deref())
        ),
        when,
        escape(&r.headline),
    ];

    let mut links = Vec::new();
    if let Some(u) = r.mfn_url.as_deref().filter(|u| !u.is_empty()) {
        links.push(format!("<{u}|MFN>"));
    }
    if let Some(u) = r.pdf_url.as_deref().filter(|u| !u.is_empty()) {
        links.push(format!("<{u}|PDF>"));
    }
    if !links.is_empty() {
        lines.push(links.join(" · "));
    }
    if r.late {
        lines.push("⏱ posted late by the daily poll (live listener missed it)".to_string());
    }
    SlackMessage {
        text: lines.join("\n"),
    }
}

/// True only when the posted item is not English and the new one is.
pub fn prefer_replacement(posted_lang: Option<&str>, new_lang: Option<&str>) -> bool {
    let is_en = |l: Option<&str>| l.is_some_and(|l| l.eq_ignore_ascii_case("en"));
    !is_en(posted_lang) && is_en(new_lang)
}

/// Which binary is posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poster {
    Live,
    Poll,
}

impl Poster {
    fn as_str(self) -> &'static str {
        match self {
            Poster::Live => "live",
            Poster::Poll => "poll",
        }
    }
}

/// Outcome of [`claim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The caller owns the post and must call [`mark_posted`] or [`mark_failed`].
    Acquired,
    /// Already posted; see [`posted_ref`] for a possible English upgrade.
    AlreadyPosted,
    /// Someone else holds a fresh claim, or the retry budget is spent.
    InFlight,
}

/// Claims the right to post a report. Exactly one concurrent caller gets
/// [`Claim::Acquired`]; a `claimed`/`failed` row older than ten minutes is taken over.
pub async fn claim(
    pool: &PgPool,
    notice: &ReportNotice,
    poster: Poster,
) -> Result<Claim, sqlx::Error> {
    let key = notice.report_key();
    let mut tx = pool.begin().await?;

    let inserted = sqlx::query(
        "INSERT INTO slack_report_posts
            (report_key, ticker, exchange_mic, news_id, lang, status, claimed_at, poster, attempts)
         VALUES ($1, $2, $3, $4, $5, 'claimed', now(), $6, 1)
         ON CONFLICT (report_key) DO NOTHING",
    )
    .bind(&key)
    .bind(&notice.ticker)
    .bind(&notice.exchange_mic)
    .bind(notice.news_id)
    .bind(&notice.lang)
    .bind(poster.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        tx.commit().await?;
        return Ok(Claim::Acquired);
    }

    let taken = sqlx::query(
        "UPDATE slack_report_posts
            SET status = 'claimed', claimed_at = now(), poster = $2, news_id = $3,
                lang = $4, attempts = attempts + 1
          WHERE report_key = $1 AND status <> 'posted' AND attempts < $5
            AND claimed_at < now() - make_interval(mins => $6)",
    )
    .bind(&key)
    .bind(poster.as_str())
    .bind(notice.news_id)
    .bind(&notice.lang)
    .bind(MAX_ATTEMPTS)
    .bind(STALE_MINUTES)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if taken == 1 {
        tx.commit().await?;
        return Ok(Claim::Acquired);
    }

    let row = sqlx::query("SELECT status, attempts FROM slack_report_posts WHERE report_key = $1")
        .bind(&key)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let status: String = row.get("status");
    let attempts: i32 = row.get("attempts");
    if status == "posted" {
        return Ok(Claim::AlreadyPosted);
    }
    if attempts >= MAX_ATTEMPTS {
        error!(report_key = %key, attempts, "slack report post abandoned after repeated failures");
    }
    Ok(Claim::InFlight)
}

/// Where a report was posted, and in which language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedRow {
    pub channel: String,
    pub ts: String,
    pub lang: Option<String>,
}

/// The stored Slack reference of a posted report, for [`prefer_replacement`] upgrades.
pub async fn posted_ref(pool: &PgPool, key: &str) -> Result<Option<PostedRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT channel, slack_ts, lang FROM slack_report_posts
          WHERE report_key = $1 AND status = 'posted'",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        Some(PostedRow {
            channel: r.get::<Option<String>, _>("channel")?,
            ts: r.get::<Option<String>, _>("slack_ts")?,
            lang: r.get("lang"),
        })
    }))
}

/// Records a successful post (or an upgrade to another twin).
pub async fn mark_posted(
    pool: &PgPool,
    key: &str,
    channel: &str,
    ts: &str,
    news_id: Uuid,
    lang: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE slack_report_posts
            SET status = 'posted', posted_at = now(), channel = $2, slack_ts = $3,
                news_id = $4, lang = $5, last_error = NULL
          WHERE report_key = $1",
    )
    .bind(key)
    .bind(channel)
    .bind(ts)
    .bind(news_id)
    .bind(lang)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a failed post; `error` is truncated to 500 characters.
pub async fn mark_failed(pool: &PgPool, key: &str, error: &str) -> Result<(), sqlx::Error> {
    let truncated: String = error.chars().take(MAX_ERROR_CHARS).collect();
    sqlx::query(
        "UPDATE slack_report_posts SET status = 'failed', last_error = $2
          WHERE report_key = $1 AND status <> 'posted'",
    )
    .bind(key)
    .bind(truncated)
    .execute(pool)
    .await?;
    Ok(())
}

/// Reference to a posted Slack message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedRef {
    pub channel: String,
    pub ts: String,
}

/// Why a Slack call failed. Never contains the token.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SlackError {
    #[error("slack api error: {0}")]
    Api(String),
    #[error("slack http status {0}")]
    Status(u16),
    #[error("slack request failed: {0}")]
    Transport(String),
}

enum Verdict {
    Retry(Duration, SlackError),
    Fatal(SlackError),
}

fn judge(err: &SlackClientError, backoff: Duration) -> Verdict {
    match err {
        SlackClientError::RateLimitError(e) => {
            let wait = e
                .retry_after
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(MAX_RETRY_AFTER_SECS));
            Verdict::Retry(wait, SlackError::Status(429))
        }
        SlackClientError::HttpError(e) if e.status_code.is_server_error() => {
            Verdict::Retry(backoff, SlackError::Status(e.status_code.as_u16()))
        }
        SlackClientError::HttpError(e) => {
            Verdict::Fatal(SlackError::Status(e.status_code.as_u16()))
        }
        SlackClientError::ApiError(e) => Verdict::Fatal(SlackError::Api(e.code.clone())),
        SlackClientError::HttpProtocolError(_)
        | SlackClientError::EndOfStream(_)
        | SlackClientError::SystemError(_) => {
            Verdict::Retry(backoff, SlackError::Transport("connection failed".into()))
        }
        _ => Verdict::Fatal(SlackError::Transport("unexpected slack response".into())),
    }
}

/// Posts to one Slack channel through `slack-morphism`, paced to the
/// library's one `chat.postMessage` per second.
pub struct SlackClient {
    http: SlackHyperClient,
    token: SlackApiToken,
    channel: String,
}

impl std::fmt::Debug for SlackClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackClient")
            .field("token", &"<redacted>")
            .field("channel", &self.channel)
            .finish()
    }
}

impl SlackClient {
    /// Client posting to `channel` with a bot token.
    pub fn new(token: impl Into<String>, channel: impl Into<String>) -> Result<Self, SlackError> {
        let connector = SlackClientHyperConnector::new()
            .map_err(|e| SlackError::Transport(e.to_string()))?
            .with_rate_control(SlackApiRateControlConfig::new());
        Ok(Self::with_client(
            slack_morphism::SlackClient::new(connector),
            token,
            channel,
        ))
    }

    /// Client on a caller-built `slack-morphism` client (custom API URL or connector).
    pub fn with_client(
        http: SlackHyperClient,
        token: impl Into<String>,
        channel: impl Into<String>,
    ) -> Self {
        Self {
            http,
            token: SlackApiToken::new(SlackApiTokenValue::new(token.into())),
            channel: channel.into(),
        }
    }

    /// Posts a new message with `chat.postMessage`.
    pub async fn post(&self, msg: &SlackMessage) -> Result<PostedRef, SlackError> {
        let req = SlackApiChatPostMessageRequest::new(
            SlackChannelId::new(self.channel.clone()),
            SlackMessageContent::new().with_text(msg.text.clone()),
        );
        self.retrying(|| async {
            let r = self
                .http
                .open_session(&self.token)
                .chat_post_message(&req)
                .await?;
            Ok(PostedRef {
                channel: r.channel.0,
                ts: r.ts.0,
            })
        })
        .await
    }

    /// Replaces a posted message with `chat.update`.
    pub async fn update(
        &self,
        at: &PostedRef,
        msg: &SlackMessage,
    ) -> Result<PostedRef, SlackError> {
        let req = SlackApiChatUpdateRequest::new(
            SlackChannelId::new(at.channel.clone()),
            SlackMessageContent::new().with_text(msg.text.clone()),
            SlackTs::new(at.ts.clone()),
        );
        self.retrying(|| async {
            let r = self
                .http
                .open_session(&self.token)
                .chat_update(&req)
                .await?;
            Ok(PostedRef {
                channel: r.channel.0,
                ts: r.ts.0,
            })
        })
        .await
    }

    async fn retrying<F, Fut>(&self, call: F) -> Result<PostedRef, SlackError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<PostedRef, SlackClientError>>,
    {
        let mut backoff = BASE_BACKOFF;
        let mut last = SlackError::Transport("no attempt made".into());
        for attempt in 1..=MAX_SEND_ATTEMPTS {
            let verdict = match tokio::time::timeout(REQUEST_TIMEOUT, call()).await {
                Ok(Ok(posted)) => return Ok(posted),
                Ok(Err(e)) => judge(&e, backoff),
                Err(_) => Verdict::Retry(backoff, SlackError::Transport("timed out".into())),
            };
            match verdict {
                Verdict::Fatal(e) => {
                    error!(error = %e, "slack rejected the call");
                    return Err(e);
                }
                Verdict::Retry(wait, e) => {
                    last = e;
                    if attempt < MAX_SEND_ATTEMPTS {
                        warn!(attempt, error = %last, "slack call failed, retrying");
                        tokio::time::sleep(wait).await;
                        backoff *= 2;
                    }
                }
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use slack_morphism::errors::{SlackClientApiError, SlackRateLimitError};

    fn notice() -> ReportNotice {
        ReportNotice {
            company_name: "FirstFarms A/S".into(),
            ticker: "FFARMS".into(),
            exchange_mic: "XCSE".into(),
            report_kind: ReportKind::Interim,
            fiscal_period: Some("2026-Q3".into()),
            published_at: Utc.with_ymd_and_hms(2026, 11, 24, 6, 30, 0).unwrap(),
            time_zone: chrono_tz::Europe::Copenhagen,
            session: Some("pre-open".into()),
            headline: "Quarterly update 1 January – 30 September 2026".into(),
            mfn_url: Some("https://mfn.se/a/x".into()),
            pdf_url: Some("https://x.test/file.pdf".into()),
            late: false,
            news_id: Uuid::from_u128(1),
            lang: Some("en".into()),
        }
    }

    #[test]
    fn full_message() {
        assert_eq!(
            format_report_message(&notice()).text,
            "📊 *FirstFarms A/S (FFARMS)* · Q3 2026 interim\n\
             24 Nov 2026 07:30 CET · pre-open\n\
             Quarterly update 1 January – 30 September 2026\n\
             <https://mfn.se/a/x|MFN> · <https://x.test/file.pdf|PDF>"
        );
    }

    #[test]
    fn zone_abbreviation_on_both_sides_of_dst() {
        let mut n = notice();
        n.published_at = Utc.with_ymd_and_hms(2026, 7, 1, 5, 30, 0).unwrap();
        assert!(format_report_message(&n)
            .text
            .contains("1 Jul 2026 07:30 CEST"));
        n.time_zone = chrono_tz::Europe::Helsinki;
        n.published_at = Utc.with_ymd_and_hms(2026, 1, 15, 5, 30, 0).unwrap();
        assert!(format_report_message(&n)
            .text
            .contains("15 Jan 2026 07:30 EET"));
        n.published_at = Utc.with_ymd_and_hms(2026, 7, 15, 4, 30, 0).unwrap();
        assert!(format_report_message(&n)
            .text
            .contains("15 Jul 2026 07:30 EEST"));
    }

    #[test]
    fn omits_missing_pieces() {
        let mut n = notice();
        n.fiscal_period = None;
        n.session = None;
        n.pdf_url = None;
        let t = format_report_message(&n).text;
        assert!(t.contains("(FFARMS)* · interim\n"));
        assert!(t.contains("07:30 CET\n"));
        assert!(t.ends_with("<https://mfn.se/a/x|MFN>"));
        n.mfn_url = None;
        let t = format_report_message(&n).text;
        assert!(t.ends_with("30 September 2026"));
        assert!(!t.contains("None") && !t.contains("null"));
    }

    #[test]
    fn late_annual_and_escaping() {
        let mut n = notice();
        n.report_kind = ReportKind::Annual;
        n.fiscal_period = Some("2025".into());
        n.late = true;
        n.headline = "Q3: EBIT <0 & falling".into();
        n.company_name = "A&B <Co>".into();
        let t = format_report_message(&n).text;
        assert!(t.contains("*A&amp;B &lt;Co&gt; (FFARMS)* · 2025 annual"));
        assert!(t.contains("Q3: EBIT &lt;0 &amp; falling"));
        assert!(t.ends_with("⏱ posted late by the daily poll (live listener missed it)"));
    }

    #[test]
    fn report_key_is_twin_stable() {
        let mut a = notice();
        let mut b = notice();
        a.lang = Some("sv".into());
        b.headline = "Delårsrapport".into();
        assert_eq!(a.report_key(), b.report_key());
        assert_eq!(a.report_key(), "FFARMS:XCSE:1795501800:interim");
    }

    #[test]
    fn replacement_truth_table() {
        let langs = [Some("sv"), Some("da"), Some("fi"), Some("en"), None];
        for posted in langs {
            for new in langs {
                let expected = posted != Some("en") && new == Some("en");
                assert_eq!(
                    prefer_replacement(posted, new),
                    expected,
                    "{posted:?} {new:?}"
                );
            }
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_lifecycle(pool: PgPool) -> sqlx::Result<()> {
        let n = notice();
        let key = n.report_key();
        assert_eq!(claim(&pool, &n, Poster::Live).await?, Claim::Acquired);
        assert_eq!(claim(&pool, &n, Poster::Poll).await?, Claim::InFlight);

        mark_failed(&pool, &key, &"x".repeat(900)).await?;
        let len: i32 = sqlx::query_scalar("SELECT length(last_error) FROM slack_report_posts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(len, 500);
        assert_eq!(claim(&pool, &n, Poster::Poll).await?, Claim::InFlight);

        sqlx::query("UPDATE slack_report_posts SET claimed_at = now() - interval '11 minutes'")
            .execute(&pool)
            .await?;
        assert_eq!(claim(&pool, &n, Poster::Poll).await?, Claim::Acquired);
        let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM slack_report_posts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(attempts, 2);

        mark_posted(&pool, &key, "C1", "1.2", n.news_id, Some("sv")).await?;
        assert_eq!(claim(&pool, &n, Poster::Live).await?, Claim::AlreadyPosted);
        let p = posted_ref(&pool, &key).await?.unwrap();
        assert_eq!(
            (p.channel.as_str(), p.ts.as_str(), p.lang.as_deref()),
            ("C1", "1.2", Some("sv"))
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stale_claim_stops_after_five_attempts(pool: PgPool) -> sqlx::Result<()> {
        let n = notice();
        assert_eq!(claim(&pool, &n, Poster::Live).await?, Claim::Acquired);
        sqlx::query("UPDATE slack_report_posts SET attempts = 5, status = 'failed', claimed_at = now() - interval '1 hour'")
            .execute(&pool)
            .await?;
        assert_eq!(claim(&pool, &n, Poster::Poll).await?, Claim::InFlight);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_claims_yield_one_winner(pool: PgPool) -> sqlx::Result<()> {
        let n = notice();
        let (a, b) = tokio::join!(
            tokio::spawn({
                let (p, n) = (pool.clone(), n.clone());
                async move { claim(&p, &n, Poster::Live).await }
            }),
            tokio::spawn({
                let (p, n) = (pool.clone(), n.clone());
                async move { claim(&p, &n, Poster::Poll).await }
            })
        );
        let got = [a.unwrap()?, b.unwrap()?];
        assert_eq!(got.iter().filter(|c| **c == Claim::Acquired).count(), 1);
        Ok(())
    }

    #[test]
    fn rate_limit_wait_is_capped_at_sixty_seconds() {
        let e = SlackClientError::RateLimitError(
            SlackRateLimitError::new().with_retry_after(Duration::from_secs(600)),
        );
        match judge(&e, BASE_BACKOFF) {
            Verdict::Retry(w, SlackError::Status(429)) => assert_eq!(w, Duration::from_secs(60)),
            _ => panic!("expected a capped retry"),
        }
    }

    #[test]
    fn api_errors_are_fatal_and_carry_the_code() {
        for code in [
            "channel_not_found",
            "not_in_channel",
            "invalid_auth",
            "missing_scope",
        ] {
            let e = SlackClientError::ApiError(SlackClientApiError::new(code.to_string()));
            assert!(
                matches!(judge(&e, BASE_BACKOFF), Verdict::Fatal(SlackError::Api(c)) if c == code)
            );
        }
    }

    #[test]
    fn debug_redacts_token() {
        let c = SlackClient::new("xoxb-secret", "C1").unwrap();
        let d = format!("{c:?}");
        assert!(!d.contains("xoxb-secret") && d.contains("redacted"));
    }
}

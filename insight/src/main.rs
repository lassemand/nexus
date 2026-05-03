use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use rmcp::{model::*, tool, Error as McpError, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, PgPool};

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
struct TradeResult {
    id: i64,
    ticker: String,
    earnings_date: NaiveDate,
    buy_date: NaiveDate,
    sell_date: Option<NaiveDate>,
    buy_price: f64,
    sell_price: f64,
    pnl: f64,
    pnl_pct: f64,
    created_at: DateTime<Utc>,
}

#[derive(Clone)]
struct InsightServer {
    pool: PgPool,
}

#[tool(tool_box)]
impl InsightServer {
    #[tool(description = "Fetch all trade results. Optionally filter by ticker symbol.")]
    async fn fetch_trade_results(
        &self,
        #[tool(param)]
        #[schemars(description = "Optional ticker symbol to filter by, e.g. AAPL")]
        ticker: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let results: Vec<TradeResult> = match ticker {
            Some(ref t) => {
                sqlx::query_as(
                    "SELECT * FROM trade_results WHERE ticker = $1 ORDER BY earnings_date DESC",
                )
                .bind(t)
                .fetch_all(&self.pool)
                .await
            }

            None => {
                sqlx::query_as("SELECT * FROM trade_results ORDER BY earnings_date DESC")
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Summarise trade results: count, total PnL, average PnL%, win rate. Optionally filter by ticker."
    )]
    async fn summarise_trade_results(
        &self,
        #[tool(param)]
        #[schemars(description = "Optional ticker symbol to filter by")]
        ticker: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let where_clause = if ticker.is_some() {
            "WHERE ticker = $1"
        } else {
            ""
        };

        let query = format!(
            r#"
            SELECT
                COUNT(*)            AS total_trades,
                SUM(pnl)            AS total_pnl,
                AVG(pnl_pct)        AS avg_pnl_pct,
                COUNT(*) FILTER (WHERE pnl > 0) AS winning_trades
            FROM trade_results
            {where_clause}
            "#
        );

        #[derive(Serialize, sqlx::FromRow)]
        struct Summary {
            total_trades: i64,
            total_pnl: Option<f64>,
            avg_pnl_pct: Option<f64>,
            winning_trades: i64,
        }

        let row: Summary = if let Some(ref t) = ticker {
            sqlx::query_as(&query).bind(t).fetch_one(&self.pool).await
        } else {
            sqlx::query_as(&query).fetch_one(&self.pool).await
        }
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let win_rate = if row.total_trades > 0 {
            row.winning_trades as f64 / row.total_trades as f64 * 100.0
        } else {
            0.0
        };

        let summary = serde_json::json!({
            "total_trades": row.total_trades,
            "winning_trades": row.winning_trades,
            "win_rate_pct": win_rate,
            "total_pnl": row.total_pnl.unwrap_or(0.0),
            "avg_pnl_pct": row.avg_pnl_pct.unwrap_or(0.0),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&summary).unwrap(),
        )]))
    }
}

#[tool(tool_box)]
impl ServerHandler for InsightServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "nexus-insight".into(),
                version: "0.1.0".into(),
            },
            instructions: Some(
                "Provides access to backtested trade results from the Nexus Alpha pipeline.".into(),
            ),
        }
    }
}

#[derive(Parser)]
#[command(about = "MCP server exposing Nexus Alpha trade results")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();

    let args = Args::parse();

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await?;

    let server = InsightServer { pool };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    Ok(())
}

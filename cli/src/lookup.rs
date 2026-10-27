//! Source-mapping lookups for `nexus register`: the MFN company (slug and
//! name) and, for Swedish listings, the issuer name in Finansinspektionen's
//! insider register. When a lookup is ambiguous the user picks from a list.

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use model::asset::mic;
use serde::Deserialize;

const MFN_BASE: &str = "https://mfn.se";
const FI_BASE: &str = "https://marknadssok.fi.se";
const FI_AUTOCOMPLETE: &str =
    "/Publiceringsklient/sv-SE/AutoComplete/H%C3%A4mtaAutoCompleteListaFull";

/// Mappings found for a listing; `None` when not found or not chosen.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mappings {
    pub name: Option<String>,
    pub mfn_slug: Option<String>,
    pub fi_issuer: Option<String>,
}

/// Values given on the command line, which skip the matching lookup.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub mfn_slug: Option<String>,
    pub fi_issuer: Option<String>,
}

/// A company as MFN's company search returns it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MfnCompany {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub tickers: Vec<String>,
}

impl MfnCompany {
    fn label(&self) -> String {
        format!(
            "{} — {} [{}]",
            self.slug,
            self.name,
            self.tickers.join(", ")
        )
    }
}

/// Whether a listing on `exchange_mic` has Nordic report sources.
pub fn is_nordic(exchange_mic: &str) -> bool {
    [mic::FNSE, mic::XSTO, mic::XCSE, mic::FNDK].contains(&exchange_mic)
}

/// Whether a listing on `exchange_mic` is covered by FI's insider register.
fn is_swedish(exchange_mic: &str) -> bool {
    mic::country(exchange_mic) == Some("SE")
}

/// Uppercase without spaces or hyphens, as `nordic-reports` compares tickers
/// (MFN's `NOVO B` matches the registry's `NOVO-B`).
fn normalise(t: &str) -> String {
    t.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

/// The companies listing `ticker` on `exchange_mic` (`MIC:TICKER` in MFN's
/// `tickers`, compared normalised).
pub fn matching<'a>(
    companies: &'a [MfnCompany],
    ticker: &str,
    exchange_mic: &str,
) -> Vec<&'a MfnCompany> {
    let want = format!("{}:{}", exchange_mic.to_uppercase(), normalise(ticker));
    companies
        .iter()
        .filter(|c| {
            c.tickers.iter().any(|t| match t.split_once(':') {
                Some((m, tk)) => format!("{}:{}", m.to_uppercase(), normalise(tk)) == want,
                None => false,
            })
        })
        .collect()
}

/// A user's answer to a numbered list of `n` options: `Some(Some(i))` for
/// option `i` (0-based), `Some(None)` for "none" (`0`), `None` when invalid.
pub fn parse_choice(input: &str, n: usize) -> Option<Option<usize>> {
    match input.trim().parse::<usize>().ok()? {
        0 => Some(None),
        i if i <= n => Some(Some(i - 1)),
        _ => None,
    }
}

/// HTTP client for the two lookups; the base URLs are swapped in tests.
pub struct Lookup {
    http: reqwest::Client,
    mfn_base: String,
    fi_base: String,
}

impl Lookup {
    /// Client for the public MFN and FI endpoints.
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_bases(http, MFN_BASE, FI_BASE)
    }

    fn with_bases(http: reqwest::Client, mfn_base: &str, fi_base: &str) -> Self {
        Self {
            http,
            mfn_base: mfn_base.trim_end_matches('/').to_string(),
            fi_base: fi_base.trim_end_matches('/').to_string(),
        }
    }

    /// MFN's company search (`/search/companies?query=`), fuzzy by name and ticker.
    pub async fn search_mfn(&self, query: &str) -> Result<Vec<MfnCompany>> {
        self.http
            .get(format!("{}/search/companies", self.mfn_base))
            .query(&[("query", query)])
            .header("User-Agent", "Mozilla/5.0")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .context("MFN company search failed")?
            .json()
            .await
            .context("unexpected MFN company search response")
    }

    /// Issuer names in FI's insider register that match `term`.
    pub async fn fi_issuers(&self, term: &str) -> Result<Vec<String>> {
        self.http
            .get(format!("{}{FI_AUTOCOMPLETE}", self.fi_base))
            .query(&[
                ("sokfunktion", "Insyn"),
                ("falt", "Utgivare"),
                ("sokterm", term),
            ])
            .header("User-Agent", "Mozilla/5.0")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .context("FI issuer search failed")?
            .json()
            .await
            .context("unexpected FI issuer search response")
    }

    /// Looks up the mappings of a Nordic listing, asking `chooser` when a
    /// lookup has several candidates. A failed lookup is reported and left
    /// empty; it never fails the registration.
    pub async fn mappings(
        &self,
        ticker: &str,
        exchange_mic: &str,
        overrides: &Overrides,
        chooser: &mut dyn Chooser,
    ) -> Mappings {
        let mut out = Mappings::default();
        if !is_nordic(exchange_mic) {
            return out;
        }
        let label = format!("{ticker} ({exchange_mic})");

        let company = match &overrides.mfn_slug {
            Some(slug) => {
                let name = match self.search_mfn(slug).await {
                    Ok(found) => found.into_iter().find(|c| &c.slug == slug).map(|c| c.name),
                    Err(e) => {
                        eprintln!("  ! {label}: {e:#}");
                        None
                    }
                };
                Some((slug.clone(), name))
            }
            None => match self.search_mfn(ticker).await {
                Ok(found) => {
                    let exact = matching(&found, ticker, exchange_mic);
                    let (candidates, why): (Vec<&MfnCompany>, &str) = match exact.len() {
                        0 => (found.iter().collect(), "no MFN company lists this ticker"),
                        1 => (exact, ""),
                        _ => (exact, "several MFN companies list this ticker"),
                    };
                    let chosen = match candidates.as_slice() {
                        [] => {
                            eprintln!(
                                "  ! {label}: not found on MFN; reports need mfn_slug (--mfn-slug)"
                            );
                            None
                        }
                        [only] if why.is_empty() => Some(*only),
                        _ => {
                            let labels: Vec<String> =
                                candidates.iter().map(|c| c.label()).collect();
                            chooser
                                .choose(&format!("{label}: {why}. MFN company"), &labels)
                                .map(|i| candidates[i])
                        }
                    };
                    chosen.map(|c| (c.slug.clone(), Some(c.name.clone())))
                }
                Err(e) => {
                    eprintln!("  ! {label}: {e:#}; reports need mfn_slug (--mfn-slug)");
                    None
                }
            },
        };
        if let Some((slug, name)) = company {
            out.mfn_slug = Some(slug);
            out.name = name;
        }

        if is_swedish(exchange_mic) {
            out.fi_issuer = match (&overrides.fi_issuer, &out.name) {
                (Some(issuer), _) => Some(issuer.clone()),
                (None, Some(name)) => match self.fi_issuers(name).await {
                    Ok(found) => match found.as_slice() {
                        [] => {
                            eprintln!(
                                "  ! {label}: no FI issuer named like '{name}' (--fi-issuer)"
                            );
                            None
                        }
                        [only] => Some(only.clone()),
                        _ => chooser
                            .choose(&format!("{label}: FI insider-register issuer"), &found)
                            .map(|i| found[i].clone()),
                    },
                    Err(e) => {
                        eprintln!("  ! {label}: {e:#} (--fi-issuer)");
                        None
                    }
                },
                (None, None) => None,
            };
        }
        out
    }
}

/// Picks one of several options, or none.
pub trait Chooser {
    /// Returns the index of the chosen option, or `None` for none.
    fn choose(&mut self, question: &str, options: &[String]) -> Option<usize>;
}

/// Asks on the terminal; without one, chooses none and lists the options so
/// they can be passed as flags.
pub struct TerminalChooser;

impl Chooser for TerminalChooser {
    fn choose(&mut self, question: &str, options: &[String]) -> Option<usize> {
        if !io::stdin().is_terminal() {
            eprintln!("  ! {question}: no terminal to ask, left empty. Options:");
            for o in options {
                eprintln!("      {o}");
            }
            return None;
        }
        tokio::task::block_in_place(|| ask(question, options))
    }
}

fn ask(question: &str, options: &[String]) -> Option<usize> {
    let stdin = io::stdin();
    loop {
        println!("{question}?");
        for (i, o) in options.iter().enumerate() {
            println!("  {}) {o}", i + 1);
        }
        println!("  0) none");
        print!("Choose [0-{}]: ", options.len());
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        match parse_choice(&line, options.len()) {
            Some(choice) => return choice,
            None => println!("Please enter a number from 0 to {}.", options.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn company(slug: &str, name: &str, tickers: &[&str]) -> MfnCompany {
        MfnCompany {
            slug: slug.into(),
            name: name.into(),
            tickers: tickers.iter().map(|t| t.to_string()).collect(),
        }
    }

    /// Answers from a script and records the questions.
    struct Scripted {
        answers: Vec<Option<usize>>,
        asked: Vec<(String, Vec<String>)>,
    }

    impl Chooser for Scripted {
        fn choose(&mut self, question: &str, options: &[String]) -> Option<usize> {
            self.asked.push((question.into(), options.to_vec()));
            self.answers.remove(0)
        }
    }

    fn scripted(answers: &[Option<usize>]) -> Scripted {
        Scripted {
            answers: answers.to_vec(),
            asked: Vec::new(),
        }
    }

    const NOVO_SEARCH: &str = r#"[
        {"slug":"novotek","name":"Novotek","tickers":["XSTO:NTEK B"]},
        {"slug":"novonesis","name":"Novonesis","tickers":["XCSE:NSIS B"]},
        {"slug":"novo-nordisk","name":"Novo Nordisk","tickers":["XCSE:NOVO B","XNYS:NVO"]}
    ]"#;

    async fn server_with(query: &str, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/companies"))
            .and(query_param("query", query))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn matching_compares_mic_and_normalised_ticker() {
        let found = [
            company("torm", "Torm", &["XCSE:TRMD A", "XNAS:TRMD"]),
            company("volvo", "Volvo", &["XSTO:VOLV A", "XSTO:VOLV B"]),
        ];
        assert_eq!(matching(&found, "TRMD-A", "XCSE")[0].slug, "torm");
        assert!(matching(&found, "TRMD-A", "FNDK").is_empty());
        assert!(matching(&found, "TRMD", "XCSE").is_empty());
        assert_eq!(matching(&found, "volv-b", "XSTO")[0].slug, "volvo");
    }

    #[test]
    fn choices_are_one_based_with_zero_for_none() {
        assert_eq!(parse_choice("2\n", 3), Some(Some(1)));
        assert_eq!(parse_choice(" 0 ", 3), Some(None));
        assert_eq!(parse_choice("4", 3), None);
        assert_eq!(parse_choice("x", 3), None);
    }

    #[tokio::test]
    async fn one_exact_match_is_taken_without_asking() {
        let server = server_with("NOVO-B", NOVO_SEARCH).await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let mut chooser = scripted(&[]);
        let m = lookup
            .mappings("NOVO-B", "XCSE", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(m.mfn_slug.as_deref(), Some("novo-nordisk"));
        assert_eq!(m.name.as_deref(), Some("Novo Nordisk"));
        assert_eq!(m.fi_issuer, None, "Danish listings have no FI issuer");
        assert!(chooser.asked.is_empty());
    }

    #[tokio::test]
    async fn without_an_exact_match_the_user_picks_from_all_results() {
        let server = server_with("NOVO-A", NOVO_SEARCH).await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let mut chooser = scripted(&[Some(1)]);
        let m = lookup
            .mappings("NOVO-A", "XCSE", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(m.mfn_slug.as_deref(), Some("novonesis"));
        assert_eq!(chooser.asked[0].1.len(), 3);

        let mut none = scripted(&[None]);
        let m = lookup
            .mappings("NOVO-A", "XCSE", &Overrides::default(), &mut none)
            .await;
        assert_eq!(m, Mappings::default());
    }

    #[tokio::test]
    async fn swedish_listings_get_an_fi_issuer_and_ask_when_several() {
        let server = server_with(
            "VOLV-B",
            r#"[{"slug":"volvo","name":"Volvo","tickers":["XSTO:VOLV A","XSTO:VOLV B"]}]"#,
        )
        .await;
        Mock::given(method("GET"))
            .and(path(FI_AUTOCOMPLETE))
            .and(query_param("sokterm", "Volvo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"["Volvo Car AB","AB Volvo (publ)"]"#, "application/json"),
            )
            .mount(&server)
            .await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let mut chooser = scripted(&[Some(1)]);
        let m = lookup
            .mappings("VOLV-B", "XSTO", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(m.mfn_slug.as_deref(), Some("volvo"));
        assert_eq!(m.fi_issuer.as_deref(), Some("AB Volvo (publ)"));
        assert_eq!(chooser.asked.len(), 1, "only the FI issuer was ambiguous");
    }

    #[tokio::test]
    async fn overrides_skip_the_lookups_and_us_listings_get_nothing() {
        let server = server_with(
            "torm",
            r#"[{"slug":"torm","name":"Torm","tickers":["XCSE:TRMD A"]}]"#,
        )
        .await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let overrides = Overrides {
            mfn_slug: Some("torm".into()),
            fi_issuer: None,
        };
        let mut chooser = scripted(&[]);
        let m = lookup
            .mappings("TRMD-A", "XCSE", &overrides, &mut chooser)
            .await;
        assert_eq!(
            (m.mfn_slug.as_deref(), m.name.as_deref()),
            (Some("torm"), Some("Torm"))
        );

        let us = lookup
            .mappings("TRMD", "XNAS", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(us, Mappings::default());
    }

    #[tokio::test]
    async fn a_failed_search_leaves_the_mappings_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let mut chooser = scripted(&[]);
        let m = lookup
            .mappings("GOMX", "FNSE", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(m, Mappings::default());
    }
}

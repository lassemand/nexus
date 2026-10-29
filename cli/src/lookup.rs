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
    pub isin: Option<String>,
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
    #[serde(default)]
    pub isins: Vec<String>,
}

impl MfnCompany {
    /// The ISIN, when the company lists exactly one valid one (several mean
    /// several share classes, and the listing's own cannot be told apart).
    fn isin(&self) -> Option<String> {
        match self
            .isins
            .iter()
            .filter(|i| model::isin::is_valid(i))
            .collect::<Vec<_>>()
            .as_slice()
        {
            [only] => Some(only.to_string()),
            _ => None,
        }
    }

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
    mic::NORDIC.contains(&exchange_mic)
}

/// Whether a listing on `exchange_mic` is covered by FI's insider register
/// (Sweden only: Finnish insider trades come from issuers' MFN releases).
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
                let found = match self.search_mfn(slug).await {
                    Ok(found) => found.into_iter().find(|c| &c.slug == slug),
                    Err(e) => {
                        eprintln!("  ! {label}: {e:#}");
                        None
                    }
                };
                Some((
                    slug.clone(),
                    found.as_ref().map(|c| c.name.clone()),
                    found.as_ref().and_then(MfnCompany::isin),
                ))
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
                    chosen.map(|c| (c.slug.clone(), Some(c.name.clone()), c.isin()))
                }
                Err(e) => {
                    eprintln!("  ! {label}: {e:#}; reports need mfn_slug (--mfn-slug)");
                    None
                }
            },
        };
        if let Some((slug, name, isin)) = company {
            out.mfn_slug = Some(slug);
            out.name = name;
            out.isin = isin;
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
            isins: Vec::new(),
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
        {"slug":"novo-nordisk","name":"Novo Nordisk","tickers":["XCSE:NOVO B","XNYS:NVO"],"isins":["DK0062498333"]}
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
    fn nordic_covers_finland_and_only_sweden_has_an_fi_register() {
        for m in ["FNSE", "XSTO", "XCSE", "FNDK", "XHEL", "FNFI"] {
            assert!(is_nordic(m), "{m}");
        }
        assert!(!is_nordic("XNAS") && !is_nordic("XOSL"));
        assert!(is_swedish("FNSE") && is_swedish("XSTO"));
        for m in ["XHEL", "FNFI", "XCSE", "FNDK", "XNAS"] {
            assert!(!is_swedish(m), "{m}");
        }
    }

    /// Two MFN companies are called Metso; only `metso-2` lists `XHEL:METSO`.
    const METSO_SEARCH: &str = r#"[
        {"slug":"metso","name":"Metso","tickers":["XHEL:NELES"],"isins":["FI4000440664"]},
        {"slug":"metso-2","name":"Metso","tickers":["XHEL:METSO","CAPA:METSOh","XLON:0MGI"],"isins":["FI0009014575"]}
    ]"#;

    #[tokio::test]
    async fn metso_maps_to_metso_2_not_the_old_metso_without_asking() {
        let server = server_with("METSO", METSO_SEARCH).await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let mut chooser = scripted(&[]);
        let m = lookup
            .mappings("METSO", "XHEL", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(m.mfn_slug.as_deref(), Some("metso-2"));
        assert_eq!(m.name.as_deref(), Some("Metso"));
        assert_eq!(m.isin.as_deref(), Some("FI0009014575"));
        assert_eq!(m.fi_issuer, None, "Finland has no FI issuer");
        assert!(chooser.asked.is_empty(), "no prompt");
    }

    #[tokio::test]
    async fn if_only_the_old_company_is_found_it_is_not_picked_but_asked() {
        let only_old = r#"[{"slug":"metso","name":"Metso","tickers":["XHEL:NELES"],"isins":["FI4000440664"]}]"#;
        let server = server_with("METSO", only_old).await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let mut chooser = scripted(&[None]);
        let m = lookup
            .mappings("METSO", "XHEL", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(m, Mappings::default(), "declined: nothing is mapped");
        assert_eq!(chooser.asked.len(), 1);
        assert!(chooser.asked[0]
            .0
            .contains("no MFN company lists this ticker"));
    }

    #[tokio::test]
    async fn finnish_tickers_match_on_their_own_mic() {
        let found = r#"[{"slug":"kne","name":"Kone","tickers":["XHEL:KNEBV"],"isins":["FI0009013403"]},
            {"slug":"sampo","name":"Sampo","tickers":["FNFI:SAMPO"],"isins":["FI4000552500"]}]"#;
        let server = server_with("KNEBV", found).await;
        let lookup = Lookup::with_bases(reqwest::Client::new(), &server.uri(), &server.uri());
        let mut chooser = scripted(&[]);
        let m = lookup
            .mappings("KNEBV", "XHEL", &Overrides::default(), &mut chooser)
            .await;
        assert_eq!(m.mfn_slug.as_deref(), Some("kne"));
        let list = [
            company("kne", "Kone", &["XHEL:KNEBV"]),
            company("sampo", "Sampo", &["FNFI:SAMPO"]),
        ];
        assert!(matching(&list, "SAMPO", "XHEL").is_empty());
        assert_eq!(matching(&list, "SAMPO", "FNFI")[0].slug, "sampo");
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
        assert_eq!(m.isin.as_deref(), Some("DK0062498333"));
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
            r#"[{"slug":"volvo","name":"Volvo","tickers":["XSTO:VOLV A","XSTO:VOLV B"],
                 "isins":["SE0000115420","SE0000115446"]}]"#,
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
        assert_eq!(m.isin, None, "two share classes, so no ISIN");
        assert_eq!(chooser.asked.len(), 1, "only the FI issuer was ambiguous");
    }

    #[tokio::test]
    async fn overrides_skip_the_lookups_and_us_listings_get_nothing() {
        let server = server_with(
            "torm",
            r#"[{"slug":"torm","name":"Torm","tickers":["XCSE:TRMD A"],"isins":["GB00BZ3CNK81"]}]"#,
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
        assert_eq!(m.isin.as_deref(), Some("GB00BZ3CNK81"));

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

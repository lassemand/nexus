//! The poll side of the company-announcement chain against real recorded MFN
//! feed pages (NEX-240): `ingest_company` fed with three fixtures, the exact set
//! of `company_announcement` events, and the captured events as a file that
//! signal's `e2e_announcements` tests replay.
//!
//! The fixtures under `tests/fixtures/mfn/e2e/` are first pages of MFN's company
//! feeds as served, trimmed to at most 30 items, with `next_url` and `prev_url`
//! set to null:
//!
//! * `firstfarms` (Danish, da+en twins, the 2026-08-26 profit warning and interim
//!   report, a calendar release, an insider report, a shareholder notice);
//! * `volvo` (Swedish, sv+en twins, untagged PR, an item without a twin);
//! * `metso` (Finnish, fi+en twins, 18 managers' transactions in one second).
//!
//! Refresh them with, for each `<slug>`, `<offset>` and item range:
//!
//! ```sh
//! curl -sS -A "nexus-chronicle/nordic-reports (<contact>)" \
//!   "https://mfn.se/all/a/<slug>.json?limit=48&offset=<offset>" \
//!   | jq '.next_url=null | .prev_url=null | .items |= .[<from>:<to>]' \
//!   > chronicle/tests/fixtures/mfn/e2e/<name>.json
//! ```
//!
//! (`firstfarms` 0 `[0:24]`, `volvo` 0 `[0:27]`, `metso-2` 48 `[6:36]`.) The
//! expected values below are literals written from the fixtures by hand-rules
//! (a `sub:report` tag is a report, `sub:ci:calendar` a calendar release,
//! everything else an announcement), not computed by the code under test: update
//! them with the fixtures. The `.events.pb` files are the events the poll emits,
//! length-delimited protobuf in emission order; rewrite them with
//! `REFRESH_E2E_FIXTURES=1 cargo test -p chronicle --test announcements_e2e`.
//! The tests themselves never touch the network.

mod common;

use async_trait::async_trait;
use chronicle_lib::nordic::calendar::MfnClient;
use chronicle_lib::nordic::db::Company;
use chronicle_lib::nordic::events::EventSink;
use chronicle_lib::nordic::ingest::ingest_company;
use common::TestDb;
use model::generated::SpecialEvent;
use prost::Message;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Hand-written from the fixtures. `Annual Report 2025: ...` (tagged
/// `sub:report:annual`, a full annual report) is classified by the existing title
/// rule as the annual-report *document*, not a results release, so it is an
/// announcement here, not a report.
const FIRSTFARMS_ANNOUNCEMENTS: &[(&str, &str, &[&str], &str)] = &[
    ("b18b9719-0289-4462-b33c-981138f45a47", "da", &[":regulatory"], "FirstFarms nedjusterer forventningerne til året grundet tørke i Slovakiet samt fald i grisepriser"),
    ("71d331d0-4f1c-431b-97dc-3120f05bbcb2", "en", &[":regulatory"], "FirstFarms adjusts downwards the expectations for the year based on drought in Slovakia and decrease in pig prices"),
    ("f4baf314-3303-40b5-b149-7f37e7c77842", "da", &[":regulatory"], "FirstFarms nedjusterer forventningerne til året baseret på faldende grisepriser samt tørke i Slovakiet og Ungarn"),
    ("63553112-9c12-4d79-989c-d2a6bdbdb930", "en", &[":regulatory"], "FirstFarms adjusts the expectation for the year based on decreasing pig prices and drought in Slovakia and Hungary"),
    ("85a9d72b-3d5f-4312-9964-405557d697b7", "da", &[":regulatory"], "Forløb af ordinær generalforsamling i FirstFarms A/S "),
    ("8eb5b950-ba4e-495b-82c6-5d38965a0c80", "en", &[":regulatory"], "Progress of annual general meeting in FirstFarms A/S"),
    ("4b3ec349-deda-4c44-a597-b7e3f24c0100", "da", &[":regulatory"], "Indkaldelse til ordinær generalforsamling i FirstFarms A/S"),
    ("192ce53a-e2cc-44a3-a0b2-f9d8504dc82c", "en", &[":regulatory"], "Notice to convene the annual general meeting in FirstFarms A/S"),
    ("e48dcb50-ee01-4650-bedd-c8ea5982b50c", "da", &[":regulatory", "sub:report", "sub:report:annual"], "Årsrapport 2025: Et særdeles udfordrende år for FirstFarms A/S"),
    ("4f3c5894-2efa-493b-a724-d85e2f5c2e9d", "en", &[":regulatory", "sub:report", "sub:report:annual"], "Annual Report 2025: An exceptionally challenging year for FirstFarms A/S"),
    ("288f1d30-d18e-4c42-9f11-7345e8029127", "da", &[":regulatory", "sub:ci", "sub:ci:shareholder-announcement"], "Storaktionærmeddelelse – AIC A/S"),
    ("7beb8ef5-1fd4-4afa-8264-2ec0ebd0b758", "en", &[":regulatory", "sub:ci", "sub:ci:shareholder-announcement"], "Major shareholder announcement - AIC A/S"),
    ("18105d66-53c7-4bc9-a3cb-cf5c100b7f97", "en", &[":regulatory", "sub:ci", "sub:ci:insider"], "Reporting of transactions made by members of the Board of Directors or Executive Management or their closely associated persons"),
    ("ac11fed7-f33d-40e9-8299-81241a30e15d", "da", &[":regulatory", "sub:ci", "sub:ci:insider"], "Indberetning af transaktioner foretaget af medlemmer af bestyrelsen eller direktionen eller disses nærtstående personer"),
    ("55ff26ba-24cc-4ce8-98a8-9ad41032ca55", "da", &[":regulatory"], "FirstFarms nedjusterer forventningerne til året baseret på faldende grisepriser"),
    ("2fe666e2-c2e9-4be3-aeb0-cbfb3cc3725b", "en", &[":regulatory"], "FirstFarms adjusts the expectation for the year based on decreasing pig prices"),
];
const FIRSTFARMS_REPORTS: &[&str] = &[
    "25851072-6ccc-422d-bd3a-ed9c3dfedd7f",
    "597cb006-bf61-4f75-90d1-3dc8f5015e87",
    "da00914d-a87f-4b48-9a24-1d595fa5462d",
    "e70906db-dff4-4442-9cfd-bdddf2d3d884",
    "2c989dfc-a63b-45f1-97da-b44f7fb05780",
    "5019094d-d5dc-48f8-88ee-cb1588864efa",
];

const VOLVO_ANNOUNCEMENTS: &[(&str, &str, &[&str], &str)] = &[
    ("594e7e98-b39c-535c-ba6b-64abbfd66237", "sv", &[":regulatory"], "Nytt antal röster i AB Volvo"),
    ("e7547d7c-c8e8-5bdb-92a2-207b9b8c82c7", "en", &[":regulatory"], "New number of votes in AB Volvo"),
    ("62525647-9420-564d-a23b-a71aef8eeadf", "en", &[], "AB Volvo: Volvo Group establishes dedicated entity to capture the growing demand in mining"),
    ("4fe84105-29c9-53a8-a68f-424ff5a108b0", "sv", &[], "AB Volvo: Volvokoncernen etablerar dedikerad enhet för att möta den växande efterfrågan inom gruvindustrin"),
    ("e2d13dd8-d54d-5f29-b91c-a045371d6148", "sv", &[], "AB Volvo: Inbjudan till press- och analytikerkonferens i Stockholm"),
    ("94a74b6b-9a5b-53ad-b613-c7d09fb890d5", "en", &[], "AB Volvo: Invitation to press and analyst conference in Stockholm"),
    ("bb85f96e-4c42-5826-984e-628b6a5f29e3", "sv", &[], "AB Volvo: Volvo och Waabi inleder kundverksamhet i Texas, med start hos Warp"),
    ("4bdbd63e-8d48-568c-aba5-f7184a26a182", "en", &[], "AB Volvo: Volvo and Waabi kick off customer operations in Texas, starting with Warp[TM]"),
    ("a389be26-847f-5b1f-80dc-946418a2a8a2", "en", &[], "AB Volvo: Three million tonnes hauled autonomously by Volvo"),
    ("3bb1793f-9961-54e5-893a-e755cb9568db", "en", &[], "AB Volvo: The science that brought green back to China's Loess Plateau awarded the 2026 Volvo Environment Prize"),
    ("50ba94ea-c4fa-57c3-a3ec-f19e62cfbd40", "sv", &[], "Kinas ödelagda Loessplatå blev grön igen - Bojie Fu får Volvo Environment Prize 2026"),
    ("14f47bd7-369b-54ee-9863-793f69ed1d99", "sv", &[], "AB Volvo: Ledande företag skalar upp hela värdekedjan för vätgasmobilitet i Europa"),
    ("75fb54c6-8d70-525f-b0bc-5a2f91fb8f3c", "en", &[], "AB Volvo: Industry leaders scale full value chain for hydrogen mobility across Europe"),
    ("cc63e64d-1ac0-5239-ae7d-477e58f2718e", "sv", &[], "AB Volvo: Volvokoncernen tillkännager initiativ för energipark som nästa steg i Mariestad"),
    ("63929309-5db4-58ca-9ac5-0f9da980a664", "en", &[], "AB Volvo: Volvo Group announces energy park initiative as next step in Mariestad"),
    ("e28c9f76-9310-518b-97f9-5d876f52acd5", "en", &[], "AB Volvo: Industry leaders to join forces to scale hydrogen mobility"),
    ("47e23e8d-5585-5f1f-a06d-3519ad4d5eb3", "sv", &[], "AB Volvo: Ledande aktörer går samman för att skala upp vätgasdriven mobilitet"),
    ("d8cab7a5-f316-51ff-b8b9-b17f8d85a483", "sv", &[":regulatory"], "Nytt antal röster i AB Volvo"),
    ("e4b4f3eb-7da4-50e9-9fc2-4b4a81439a66", "en", &[":regulatory"], "New number of votes in AB Volvo"),
    ("c4d0f23a-f943-5520-9598-5a33c7b7ddd0", "sv", &[], "AB Volvo: Toyota Motor Corporation, Volvokoncernen och Daimler Truck har undertecknat bindande avtal om att Toyota går in i cellcentric som likvärdig aktieägare"),
    ("1f7ce5a5-31d7-53eb-b751-351580df630d", "en", &[], "AB Volvo: Toyota Motor Corporation, Volvo Group and Daimler Truck have signed binding agreement for Toyota to join cellcentric as equal shareholder"),
    ("2c9ce1f4-2f0e-5a5f-a3fd-30d2b81d9534", "en", &[], "AB Volvo: Invitation to the Volvo Group report on the second quarter 2026"),
    ("46909639-38f3-50f7-a9f2-9ee4a83625ec", "sv", &[], "AB Volvo: Inbjudan till Volvokoncernens rapport för andra kvartalet 2026"),
];
const VOLVO_REPORTS: &[&str] = &[
    "84dc5b2b-4ee5-5e43-a755-d570338896b2",
    "62375027-86f5-5b73-b467-8e481d2bb5d3",
    "f900d8ad-959d-57c2-91e4-ce4a7e747865",
    "65c1b68c-875e-578d-acc8-ca05ed0ea99c",
];

const METSO_ANNOUNCEMENTS: &[(&str, &str, &[&str], &str)] = &[
    ("3a8ab8d8-89dd-541a-8f7f-47c29e3392e6", "fi", &[":regulatory"], "Arvopaperimarkkinalain 9 luvun 5 ja 6 §:n mukainen ilmoitus: BlackRock, Inc:n omistus Metsosta"),
    ("d3cf62d2-93ce-5e65-911a-1316f81f34d5", "en", &[":regulatory"], "Notification according to chapter 9, section 5 and 6 of the Securities Market Act: BlackRock Inc.'s holding in Metso"),
    ("61dddab4-8a61-57c3-994c-ae3d14df6b92", "fi", &[":regulatory"], "Arvopaperimarkkinalain 9 luvun 5 ja 6 §:n mukainen ilmoitus: BlackRock, Inc:n omistus Metsosta"),
    ("ec749d8f-101e-5209-9b7b-a204a0bcd445", "en", &[":regulatory"], "Notification according to chapter 9, section 5 and 6 of the Securities Market Act: BlackRock Inc.'s holding in Metso"),
    ("2bcae85a-1271-58b8-8ea1-ab427579b1a5", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Niko Pakalén"),
    ("40b5d30f-9ec5-50a8-a795-fed47ab1994a", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Terhi Koipijärvi"),
    ("8a2e6288-be8c-51e1-8ceb-084d814ce8a8", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Petra Sundström"),
    ("c1c2e10a-9358-5a19-8a56-af5c66623248", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Matts Rosenberg"),
    ("86de9d3d-59f1-50f6-880d-3b7bf01a3304", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Eriikka Söderström"),
    ("3ad24f89-789d-56eb-8788-e3aa440b5e88", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Arja Talma"),
    ("0915c0ba-5a9d-5c7e-a193-d997c45833b9", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Arja Talma"),
    ("55fd533a-3856-561c-a818-2fe74d8c6ea0", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Terhi Koipijärvi"),
    ("fc062949-0220-5e16-bfe4-806836cbe56e", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Petra Sundström"),
    ("835eee7d-6e25-5f1e-a819-4097989d6466", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Eriikka Söderström"),
    ("fb224183-2fa6-5a36-b33f-5f2cce425618", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Klaus Cawén"),
    ("e7fbf691-58e2-5f06-a5ef-74f698643f43", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Anders Svensson"),
    ("eaacae97-c974-5af1-bb1b-69552adfd046", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Kari Stadigh"),
    ("75cbd683-ddf4-5148-b4cb-9c454d9033e0", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Matts Rosenberg"),
    ("27e09b11-39fd-57c0-87d4-af810e06b621", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Niko Pakalén"),
    ("ac1d7ebd-e901-5f01-a88b-9fe872634deb", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj: Johtohenkilöiden liiketoimet - Klaus Cawén"),
    ("5d23c2a0-758d-5e72-bd6c-8a9e9d57e437", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Kari Stadigh"),
    ("3733cbb2-d7e5-5727-87fc-85a969e5616f", "en", &[":regulatory"], "Metso Corporation: Managers' transactions - Anders Svensson"),
    ("0594e2fc-a8ac-5d7f-82fa-e2b78bde5c14", "fi", &[":regulatory"], "Metso Corporation: Metson hallituksen valiokunnat"),
    ("c3e43e3d-e524-5e35-bed0-84b9830a64ef", "en", &[":regulatory"], "Metso Corporation: Committees of Metso's Board of Directors"),
    ("196cb025-3ccd-5bfc-86b6-65f8b50805b9", "en", &[":regulatory"], "Metso Corporation: Resolutions of Metso's Annual General Meeting"),
    ("495621ae-874b-513b-a6de-3b62e66e7d1e", "fi", &[":regulatory"], "Metso Corporation: Metso Oyj:n varsinaisen yhtiökokouksen päätökset"),
    ("6bf7d46a-3d55-57cf-9125-fb58c12a1584", "fi", &[":regulatory"], "Metso Corporation: Jonathan Allen nimitetty Metson Chief Growth Officeriksi"),
    ("9d2bddb4-0720-5ce0-a148-823fc3b7e289", "en", &[":regulatory"], "Metso Corporation: Metso appoints Jonathan Allen as Chief Growth Officer"),
];
const METSO_REPORTS: &[&str] = &[
    "2ffc6ee6-7cb6-5487-a534-5c077692e5d4",
    "f94590e2-a0c1-5758-a1cd-148740ddd9bd",
];

#[derive(Default)]
struct VecSink {
    sent: Mutex<Vec<SpecialEvent>>,
}

#[async_trait]
impl EventSink for VecSink {
    async fn send(&self, _key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(event.clone());
        Ok(())
    }
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mfn/e2e")
}

struct Case {
    name: &'static str,
    slug: &'static str,
    ticker: &'static str,
    mic: &'static str,
    isin: Option<&'static str>,
}

const FIRSTFARMS: Case = Case {
    name: "firstfarms",
    slug: "firstfarms",
    ticker: "FFARMS",
    mic: "XCSE",
    isin: None,
};
const VOLVO: Case = Case {
    name: "volvo",
    slug: "volvo",
    ticker: "VOLV B",
    mic: "XSTO",
    isin: None,
};
const METSO: Case = Case {
    name: "metso",
    slug: "metso-2",
    ticker: "METSO",
    mic: "XHEL",
    isin: Some("FI0009014575"),
};

/// Polls the fixture of `case` once and returns every event it emitted, in order.
async fn poll(case: &Case) -> Option<Vec<SpecialEvent>> {
    let db = TestDb::create().await?;
    let body = std::fs::read_to_string(fixture_dir().join(format!("{}.json", case.name))).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/all/a/{}.json", case.slug)))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/json"),
        )
        .mount(&server)
        .await;
    let company = Company {
        ticker: case.ticker.into(),
        exchange_mic: case.mic.into(),
        mfn_slug: Some(case.slug.into()),
        name: Some(case.name.into()),
        isin: case.isin.map(String::from),
    };
    match case.isin {
        Some(isin) => {
            db.add_company_with_isin(case.ticker, case.mic, case.slug, "EUR", isin)
                .await
        }
        None => db.add_company(case.ticker, case.mic, case.slug).await,
    }
    let sink = VecSink::default();
    let insiders = common::VecInsiders::default();
    let mut feed = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    ingest_company(
        &db.pool, &mut feed, &sink, &insiders, &company, case.slug, false,
    )
    .await
    .unwrap();
    db.drop_db().await;
    let sent = std::mem::take(&mut *sink.sent.lock().unwrap());
    Some(sent)
}

fn announcements(events: &[SpecialEvent]) -> Vec<(String, String, Vec<String>, String)> {
    events
        .iter()
        .filter(|e| e.event_type == "company_announcement")
        .map(|e| {
            (
                e.source_id.clone(),
                e.lang.clone(),
                e.tags.clone(),
                e.description.clone(),
            )
        })
        .collect()
}

fn expected(want: &[(&str, &str, &[&str], &str)]) -> Vec<(String, String, Vec<String>, String)> {
    want.iter()
        .map(|(id, lang, tags, title)| {
            (
                id.to_string(),
                lang.to_string(),
                tags.iter().map(|t| t.to_string()).collect(),
                title.to_string(),
            )
        })
        .collect()
}

fn check(events: &[SpecialEvent], want: &[(&str, &str, &[&str], &str)], reports: &[&str]) {
    assert_eq!(announcements(events), expected(want), "announcement events");

    let reported: BTreeSet<&str> = events
        .iter()
        .filter(|e| {
            matches!(
                e.event_type.as_str(),
                "quarterly_report" | "annual_report" | "report_correction"
            )
        })
        .map(|e| e.source_id.as_str())
        .collect();
    assert_eq!(
        reported,
        reports.iter().copied().collect::<BTreeSet<_>>(),
        "every report still produces its report event"
    );
    let announced: BTreeSet<&str> = want.iter().map(|w| w.0).collect();
    assert!(
        announced.is_disjoint(&reported),
        "no report is also an announcement"
    );
    assert!(
        events
            .iter()
            .filter(|e| e.event_type == "company_announcement")
            .all(|e| e.report_kind.is_empty() && e.calendar.is_none() && e.source == "mfn"),
        "announcements carry no report kind and no calendar"
    );
    assert!(
        events
            .iter()
            .filter(|e| e.event_type == "report_calendar_release")
            .all(|e| !announced.contains(e.source_id.as_str())),
        "no calendar release is also an announcement"
    );
}

#[tokio::test]
async fn danish_feed_with_da_en_twins() {
    let Some(events) = poll(&FIRSTFARMS).await else {
        return;
    };
    check(&events, FIRSTFARMS_ANNOUNCEMENTS, FIRSTFARMS_REPORTS);
    let calendar: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "report_calendar_release")
        .collect();
    assert_eq!(
        calendar.len(),
        2,
        "the da and en financial calendar releases"
    );
    let warning = events
        .iter()
        .find(|e| e.source_id == "71d331d0-4f1c-431b-97dc-3120f05bbcb2")
        .unwrap();
    assert_eq!(
        warning.occurred_at_unix_secs, 1787724921,
        "2026-08-26T06:15:21Z"
    );
    assert!(
        !warning.text.is_empty(),
        "the release text travels with the event"
    );
}

#[tokio::test]
async fn swedish_feed_with_sv_en_twins_and_untagged_press_releases() {
    let Some(events) = poll(&VOLVO).await else {
        return;
    };
    check(&events, VOLVO_ANNOUNCEMENTS, VOLVO_REPORTS);
    assert!(
        events
            .iter()
            .filter(|e| e.event_type == "company_announcement")
            .any(|e| e.tags.is_empty()),
        "untagged press releases are announcements"
    );
}

#[tokio::test]
async fn finnish_feed_with_managers_transactions() {
    let Some(events) = poll(&METSO).await else {
        return;
    };
    check(&events, METSO_ANNOUNCEMENTS, METSO_REPORTS);
    let burst = events
        .iter()
        .filter(|e| e.event_type == "company_announcement" && e.occurred_at_unix_secs == 1777035600)
        .count();
    assert_eq!(
        burst, 18,
        "the 18 managers' transaction releases of 2026-04-24T13:00:00Z"
    );
}

#[tokio::test]
async fn the_captured_events_match_the_committed_files() {
    for case in [&FIRSTFARMS, &VOLVO, &METSO] {
        let Some(events) = poll(case).await else {
            return;
        };
        let mut bytes = Vec::new();
        for e in &events {
            e.encode_length_delimited(&mut bytes).unwrap();
        }
        let file = fixture_dir().join(format!("{}.events.pb", case.name));
        if std::env::var("REFRESH_E2E_FIXTURES").is_ok() {
            std::fs::write(&file, &bytes).unwrap();
            continue;
        }
        let committed = std::fs::read(&file).unwrap_or_default();
        assert!(
            committed == bytes,
            "{} is out of date: rerun with REFRESH_E2E_FIXTURES=1",
            file.display()
        );
    }
}

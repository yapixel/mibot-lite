use crate::kit;
use chrono::{DateTime, Datelike, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::Client;
use mibot_command::{CmdError, CmdResult, Command, Invocation, code, escape, group};
use mibot_httpx::get_json;
use mibot_store::Store;
use regex::Regex;
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

const ACN_DEFAULT_ORDER: &str = "name,time";
const ACN_COMPONENTS: &[&str] = &["name", "text", "time", "weather", "emoji", "timezone"];
const ACN_STYLES: &[&str] = &["normal", "italic", "double", "sans", "mono", "outline"];

fn show_defaults(mode: &str) -> Vec<String> {
    match mode {
        "text" | "both" => vec!["text".into(), "time".into()],
        _ => vec!["time".into()],
    }
}

fn deserialize_acn_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::Null => Ok(String::new()),
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(de::Error::custom("expected string, number, or null for user_id")),
    }
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Debug, Clone)]
struct AcnUser {
    user_id: String,
    timezone: String,
    original_first_name: String,
    original_last_name: String,
    enabled: bool,
    mode: String,
    last_update: String,
    text_index: isize,
    show_clock_emoji: bool,
    show_time: Option<bool>,
    hour_format: String,
    show_timezone: bool,
    timezone_format: String,
    display_order: String,
    display_components: Option<Vec<String>>,
    text_style: String,
    weather_enabled: bool,
    weather_location: String,
    weather_compact: String,
    weather_cache_ts: i64,
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct AcnUserPlain {
    #[serde(default, deserialize_with = "deserialize_acn_id")]
    user_id: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    timezone: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    original_first_name: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    original_last_name: String,
    #[serde(rename = "is_enabled")]
    enabled: bool,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    mode: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    last_update: String,
    text_index: isize,
    show_clock_emoji: bool,
    show_time: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "deserialize_nullable_string")]
    hour_format: String,
    show_timezone: bool,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    timezone_format: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    display_order: String,
    #[serde(rename = "displayComponents", default, skip_serializing_if = "Option::is_none")]
    display_components: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    text_style: String,
    weather_enabled: bool,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    weather_location: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    weather_compact: String,
    weather_cache_ts: i64,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl From<AcnUserPlain> for AcnUser {
    fn from(p: AcnUserPlain) -> Self {
        Self {
            user_id: p.user_id,
            timezone: p.timezone,
            original_first_name: p.original_first_name,
            original_last_name: p.original_last_name,
            enabled: p.enabled,
            mode: p.mode,
            last_update: p.last_update,
            text_index: p.text_index,
            show_clock_emoji: p.show_clock_emoji,
            show_time: p.show_time,
            hour_format: p.hour_format,
            show_timezone: p.show_timezone,
            timezone_format: p.timezone_format,
            display_order: p.display_order,
            display_components: p.display_components,
            text_style: p.text_style,
            weather_enabled: p.weather_enabled,
            weather_location: p.weather_location,
            weather_compact: p.weather_compact,
            weather_cache_ts: p.weather_cache_ts,
            extra: p.extra,
        }
    }
}

impl From<&AcnUser> for AcnUserPlain {
    fn from(u: &AcnUser) -> Self {
        Self {
            user_id: u.user_id.clone(),
            timezone: u.timezone.clone(),
            original_first_name: u.original_first_name.clone(),
            original_last_name: u.original_last_name.clone(),
            enabled: u.enabled,
            mode: u.mode.clone(),
            last_update: u.last_update.clone(),
            text_index: u.text_index,
            show_clock_emoji: u.show_clock_emoji,
            show_time: u.show_time,
            hour_format: u.hour_format.clone(),
            show_timezone: u.show_timezone,
            timezone_format: u.timezone_format.clone(),
            display_order: u.display_order.clone(),
            display_components: u.display_components.clone(),
            text_style: u.text_style.clone(),
            weather_enabled: u.weather_enabled,
            weather_location: u.weather_location.clone(),
            weather_compact: u.weather_compact.clone(),
            weather_cache_ts: u.weather_cache_ts,
            extra: u.extra.clone(),
        }
    }
}

impl Serialize for AcnUser {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        AcnUserPlain::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AcnUser {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(AcnUserPlain::deserialize(deserializer)?.into())
    }
}

impl Default for AcnUser {
    fn default() -> Self {
        Self::new("")
    }
}

impl AcnUser {
    fn new(user_id: &str) -> Self {
        Self {
            user_id: user_id.to_owned(),
            timezone: "Asia/Shanghai".into(),
            original_first_name: String::new(),
            original_last_name: String::new(),
            enabled: false,
            mode: "time".into(),
            last_update: String::new(),
            text_index: 0,
            show_clock_emoji: false,
            show_time: Some(true),
            hour_format: "24".into(),
            show_timezone: false,
            timezone_format: "GMT".into(),
            display_order: ACN_DEFAULT_ORDER.into(),
            display_components: None,
            text_style: "normal".into(),
            weather_enabled: false,
            weather_location: String::new(),
            weather_compact: String::new(),
            weather_cache_ts: 0,
            extra: BTreeMap::new(),
        }
    }

    fn show_time(&self) -> bool {
        self.show_time.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct AcnState {
    #[serde(rename = "schemaVersion")]
    schema_version: i32,
    users: BTreeMap<String, Option<serde_json::Value>>,
    random_texts: Vec<String>,
}

impl Default for AcnState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            users: BTreeMap::new(),
            random_texts: Vec::new(),
        }
    }
}

impl PartialEq for AcnUser {
    fn eq(&self, other: &Self) -> bool {
        self.user_id == other.user_id
            && self.timezone == other.timezone
            && self.original_first_name == other.original_first_name
            && self.original_last_name == other.original_last_name
            && self.enabled == other.enabled
            && self.mode == other.mode
            && self.last_update == other.last_update
            && self.text_index == other.text_index
            && self.show_clock_emoji == other.show_clock_emoji
            && self.show_time == other.show_time
            && self.hour_format == other.hour_format
            && self.show_timezone == other.show_timezone
            && self.timezone_format == other.timezone_format
            && self.display_order == other.display_order
            && self.display_components == other.display_components
            && self.text_style == other.text_style
            && self.weather_enabled == other.weather_enabled
            && self.weather_location == other.weather_location
            && self.weather_compact == other.weather_compact
            && self.weather_cache_ts == other.weather_cache_ts
            && self.extra == other.extra
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct AcnDoc {
    #[serde(rename = "schemaVersion")]
    schema_version: i32,
    users: HashMap<String, AcnUser>,
    random_texts: Vec<String>,
}

impl Default for AcnDoc {
    fn default() -> Self {
        Self {
            schema_version: 1,
            users: HashMap::new(),
            random_texts: Vec::new(),
        }
    }
}

fn valid_mode(mode: &str) -> bool {
    matches!(mode, "time" | "text" | "both")
}

fn valid_zone(zone: &str) -> bool {
    let trimmed = zone.trim();
    !trimmed.is_empty() && trimmed.parse::<Tz>().is_ok()
}

fn clean_components(components: Option<Vec<String>>) -> Option<Vec<String>> {
    let items = components?;
    let mut cleaned = Vec::new();
    for c in items {
        if ACN_COMPONENTS.contains(&c.as_str()) && !cleaned.contains(&c) {
            cleaned.push(c);
        }
    }
    Some(cleaned)
}

fn normalize_user(id: &str, user: &mut AcnUser) {
    user.user_id = id.to_owned();
    if !valid_zone(&user.timezone) {
        user.timezone = "Asia/Shanghai".into();
    }
    if !valid_mode(&user.mode) {
        user.mode = "time".into();
    }
    if user.hour_format != "12" {
        user.hour_format = "24".into();
    }
    if !ACN_STYLES.contains(&user.text_style.as_str()) {
        user.text_style = "normal".into();
    }
    if user.text_index < 0 {
        user.text_index = 0;
    }
    if user.original_first_name.is_empty() {
        user.enabled = false;
    }
    user.display_components = clean_components(user.display_components.take());
}

fn clock_emoji(zone: &str, now: DateTime<Utc>) -> String {
    let Ok(tz) = zone.parse::<Tz>() else {
        return String::new();
    };
    let hour = now.with_timezone(&tz).hour() % 12;
    let cp = 0x1F550 + ((hour + 11) % 12);
    char::from_u32(cp).map(|c| c.to_string()).unwrap_or_default()
}

fn zone_time(zone: &str, hour_format: &str, now: DateTime<Utc>) -> String {
    let Ok(tz) = zone.parse::<Tz>() else {
        return String::new();
    };
    let local = now.with_timezone(&tz);
    if hour_format == "12" {
        local.format("%I:%M %p").to_string()
    } else {
        local.format("%H:%M").to_string()
    }
}

fn zone_abbreviations() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            ("Africa/Abidjan", "GMT"), ("Africa/Accra", "GMT"), ("Africa/Addis_Ababa", "EAT"), ("Africa/Algiers", "CET"),
            ("Africa/Asmara", "EAT"), ("Africa/Bamako", "GMT"), ("Africa/Bangui", "WAT"), ("Africa/Banjul", "GMT"),
            ("Africa/Bissau", "GMT"), ("Africa/Blantyre", "CAT"), ("Africa/Brazzaville", "WAT"), ("Africa/Cairo", "EET"),
            ("Africa/Casablanca", "WET"), ("Africa/Ceuta", "CET"), ("Africa/Dakar", "GMT"), ("Africa/Dar_es_Salaam", "EAT"),
            ("Africa/Djibouti", "EAT"), ("Africa/Douala", "WAT"), ("Africa/El_Aaiun", "WET"), ("Africa/Freetown", "GMT"),
            ("Africa/Gaborone", "CAT"), ("Africa/Harare", "CAT"), ("Africa/Johannesburg", "SAST"), ("Africa/Juba", "CAT"),
            ("Africa/Kampala", "EAT"), ("Africa/Khartoum", "CAT"), ("Africa/Kigali", "CAT"), ("Africa/Kinshasa", "WAT"),
            ("Africa/Lagos", "WAT"), ("Africa/Libreville", "WAT"), ("Africa/Lome", "GMT"), ("Africa/Luanda", "WAT"),
            ("Africa/Lubumbashi", "CAT"), ("Africa/Lusaka", "CAT"), ("Africa/Malabo", "WAT"), ("Africa/Maputo", "CAT"),
            ("Africa/Maseru", "SAST"), ("Africa/Mbabane", "SAST"), ("Africa/Mogadishu", "EAT"), ("Africa/Monrovia", "GMT"),
            ("Africa/Nairobi", "EAT"), ("Africa/Ndjamena", "WAT"), ("Africa/Niamey", "WAT"), ("Africa/Nouakchott", "GMT"),
            ("Africa/Ouagadougou", "GMT"), ("Africa/Porto-Novo", "WAT"), ("Africa/Sao_Tome", "GMT"), ("Africa/Tripoli", "EET"),
            ("Africa/Tunis", "CET"), ("Africa/Windhoek", "CAT"), ("America/Adak", "HST"), ("America/Anchorage", "AKST"),
            ("America/Anguilla", "AST"), ("America/Antigua", "AST"), ("America/Araguaina", "BRT"), ("America/Argentina/Buenos_Aires", "ART"),
            ("America/Argentina/Catamarca", "ART"), ("America/Argentina/Cordoba", "ART"), ("America/Argentina/Mendoza", "ART"), ("America/Aruba", "AST"),
            ("America/Asuncion", "PYT"), ("America/Atikokan", "EST"), ("America/Bahia", "BRT"), ("America/Bahia_Banderas", "CST"),
            ("America/Barbados", "AST"), ("America/Belem", "BRT"), ("America/Belize", "CST"), ("America/Blanc-Sablon", "AST"),
            ("America/Boa_Vista", "AMT"), ("America/Bogota", "COT"), ("America/Boise", "MST"), ("America/Cambridge_Bay", "MST"),
            ("America/Campo_Grande", "AMT"), ("America/Cancun", "EST"), ("America/Caracas", "VET"), ("America/Cayenne", "GFT"),
            ("America/Cayman", "EST"), ("America/Chicago", "CST"), ("America/Chihuahua", "MST"), ("America/Ciudad_Juarez", "MST"),
            ("America/Costa_Rica", "CST"), ("America/Creston", "MST"), ("America/Cuiaba", "AMT"), ("America/Curacao", "AST"),
            ("America/Danmarkshavn", "GMT"), ("America/Dawson", "MST"), ("America/Dawson_Creek", "MST"), ("America/Denver", "MST"),
            ("America/Detroit", "EST"), ("America/Dominica", "AST"), ("America/Edmonton", "MST"), ("America/Eirunepe", "ACT"),
            ("America/El_Salvador", "CST"), ("America/Fort_Nelson", "MST"), ("America/Fortaleza", "BRT"), ("America/Glace_Bay", "AST"),
            ("America/Goose_Bay", "AST"), ("America/Grand_Turk", "EST"), ("America/Guatemala", "CST"), ("America/Guayaquil", "ECT"),
            ("America/Guyana", "GYT"), ("America/Halifax", "AST"), ("America/Havana", "CST"), ("America/Hermosillo", "MST"),
            ("America/Indiana/Indianapolis", "EST"), ("America/Indiana/Knox", "CST"), ("America/Indiana/Marengo", "EST"), ("America/Indiana/Petersburg", "EST"),
            ("America/Indiana/Tell_City", "CST"), ("America/Indiana/Vevay", "EST"), ("America/Indiana/Vincennes", "EST"), ("America/Indiana/Winamac", "EST"),
            ("America/Inuvik", "MST"), ("America/Iqaluit", "EST"), ("America/Jamaica", "EST"), ("America/Juneau", "AKST"),
            ("America/Kentucky/Louisville", "EST"), ("America/Kentucky/Monticello", "EST"), ("America/Kralendijk", "AST"), ("America/La_Paz", "BOT"),
            ("America/Lima", "PET"), ("America/Los_Angeles", "PST"), ("America/Maceio", "BRT"), ("America/Managua", "CST"),
            ("America/Manaus", "AMT"), ("America/Martinique", "AST"), ("America/Matamoros", "CST"), ("America/Mazatlan", "MST"),
            ("America/Menominee", "CST"), ("America/Merida", "CST"), ("America/Metlakatla", "AKST"), ("America/Mexico_City", "CST"),
            ("America/Miquelon", "PMST"), ("America/Moncton", "AST"), ("America/Monterrey", "CST"), ("America/Montevideo", "UYT"),
            ("America/Nassau", "EST"), ("America/New_York", "EST"), ("America/Nipigon", "EST"), ("America/Nome", "AKST"),
            ("America/Noronha", "FNT"), ("America/North_Dakota/Beulah", "CST"), ("America/North_Dakota/Center", "CST"), ("America/North_Dakota/New_Salem", "CST"),
            ("America/Nuuk", "WGT"), ("America/Ojinaga", "CST"), ("America/Panama", "EST"), ("America/Paramaribo", "SRT"),
            ("America/Phoenix", "MST"), ("America/Port_of_Spain", "AST"), ("America/Port-au-Prince", "EST"), ("America/Porto_Velho", "AMT"),
            ("America/Puerto_Rico", "AST"), ("America/Punta_Arenas", "CLT"), ("America/Rainy_River", "CST"), ("America/Rankin_Inlet", "CST"),
            ("America/Recife", "BRT"), ("America/Regina", "CST"), ("America/Resolute", "CST"), ("America/Rio_Branco", "ACT"),
            ("America/Santarem", "BRT"), ("America/Santiago", "CLT"), ("America/Santo_Domingo", "AST"), ("America/Sao_Paulo", "BRT"),
            ("America/Scoresbysund", "EGT"), ("America/Sitka", "AKST"), ("America/St_Johns", "NST"), ("America/Swift_Current", "CST"),
            ("America/Tegucigalpa", "CST"), ("America/Thule", "AST"), ("America/Thunder_Bay", "EST"), ("America/Tijuana", "PST"),
            ("America/Toronto", "EST"), ("America/Tortola", "AST"), ("America/Vancouver", "PST"), ("America/Whitehorse", "MST"),
            ("America/Winnipeg", "CST"), ("America/Yakutat", "AKST"), ("America/Yellowknife", "MST"), ("Antarctica/Casey", "AWST"),
            ("Antarctica/Davis", "DAVT"), ("Antarctica/DumontDUrville", "DDUT"), ("Antarctica/Macquarie", "AEST"), ("Antarctica/Mawson", "MAWT"),
            ("Antarctica/McMurdo", "NZST"), ("Antarctica/Palmer", "CLT"), ("Antarctica/Rothera", "ROT"), ("Antarctica/South_Pole", "NZST"),
            ("Antarctica/Syowa", "SYOT"), ("Antarctica/Troll", "UTC"), ("Antarctica/Vostok", "VOST"), ("Asia/Aden", "AST"),
            ("Asia/Almaty", "ALMT"), ("Asia/Amman", "EET"), ("Asia/Anadyr", "ANAT"), ("Asia/Aqtau", "AQTT"),
            ("Asia/Aqtobe", "AQTT"), ("Asia/Ashgabat", "TMT"), ("Asia/Atyrau", "AQTT"), ("Asia/Baghdad", "AST"),
            ("Asia/Bahrain", "AST"), ("Asia/Baku", "AZT"), ("Asia/Bangkok", "ICT"), ("Asia/Barnaul", "KRAT"),
            ("Asia/Beirut", "EET"), ("Asia/Bishkek", "KGT"), ("Asia/Brunei", "BNT"), ("Asia/Calcutta", "IST"),
            ("Asia/Chita", "YAKT"), ("Asia/Choibalsan", "CHOT"), ("Asia/Chongqing", "CST"), ("Asia/Colombo", "IST"),
            ("Asia/Damascus", "EET"), ("Asia/Dhaka", "BDT"), ("Asia/Dili", "TLT"), ("Asia/Dubai", "GST"),
            ("Asia/Dushanbe", "TJT"), ("Asia/Famagusta", "EET"), ("Asia/Gaza", "EET"), ("Asia/Harbin", "CST"),
            ("Asia/Hebron", "EET"), ("Asia/Ho_Chi_Minh", "ICT"), ("Asia/Hong_Kong", "HKT"), ("Asia/Hovd", "HOVT"),
            ("Asia/Irkutsk", "IRKT"), ("Asia/Istanbul", "TRT"), ("Asia/Jakarta", "WIB"), ("Asia/Jayapura", "WIT"),
            ("Asia/Jerusalem", "IST"), ("Asia/Kabul", "AFT"), ("Asia/Kamchatka", "PETT"), ("Asia/Karachi", "PKT"),
            ("Asia/Kashgar", "XJT"), ("Asia/Kathmandu", "NPT"), ("Asia/Khandyga", "YAKT"), ("Asia/Kolkata", "IST"),
            ("Asia/Krasnoyarsk", "KRAT"), ("Asia/Kuala_Lumpur", "MYT"), ("Asia/Kuching", "MYT"), ("Asia/Kuwait", "AST"),
            ("Asia/Macao", "CST"), ("Asia/Magadan", "MAGT"), ("Asia/Makassar", "WITA"), ("Asia/Manila", "PST"),
            ("Asia/Muscat", "GST"), ("Asia/Nicosia", "EET"), ("Asia/Novokuznetsk", "KRAT"), ("Asia/Novosibirsk", "NOVT"),
            ("Asia/Omsk", "OMST"), ("Asia/Oral", "ORAT"), ("Asia/Phnom_Penh", "ICT"), ("Asia/Pontianak", "WIB"),
            ("Asia/Pyongyang", "KST"), ("Asia/Qatar", "AST"), ("Asia/Qostanay", "QYZT"), ("Asia/Qyzylorda", "QYZT"),
            ("Asia/Rangoon", "MMT"), ("Asia/Riyadh", "AST"), ("Asia/Sakhalin", "SAKT"), ("Asia/Samarkand", "UZT"),
            ("Asia/Seoul", "KST"), ("Asia/Shanghai", "CST"), ("Asia/Singapore", "SGT"), ("Asia/Srednekolymsk", "SRET"),
            ("Asia/Taipei", "CST"), ("Asia/Tashkent", "UZT"), ("Asia/Tbilisi", "GET"), ("Asia/Tehran", "IRST"),
            ("Asia/Tel_Aviv", "IST"), ("Asia/Thimphu", "BTT"), ("Asia/Tokyo", "JST"), ("Asia/Tomsk", "TOMT"),
            ("Asia/Ulaanbaatar", "ULAT"), ("Asia/Urumqi", "XJT"), ("Asia/Ust-Nera", "VLAT"), ("Asia/Vientiane", "ICT"),
            ("Asia/Vladivostok", "VLAT"), ("Asia/Yakutsk", "YAKT"), ("Asia/Yangon", "MMT"), ("Asia/Yekaterinburg", "YEKT"),
            ("Asia/Yerevan", "AMT"), ("Atlantic/Azores", "AZOT"), ("Atlantic/Bermuda", "AST"), ("Atlantic/Canary", "WET"),
            ("Atlantic/Cape_Verde", "CVT"), ("Atlantic/Faeroe", "WET"), ("Atlantic/Faroe", "WET"), ("Atlantic/Jan_Mayen", "CET"),
            ("Atlantic/Madeira", "WET"), ("Atlantic/Reykjavik", "GMT"), ("Atlantic/South_Georgia", "GST"), ("Atlantic/St_Helena", "GMT"),
            ("Atlantic/Stanley", "FKST"), ("Arctic/Longyearbyen", "CET"), ("Australia/ACT", "AEST"), ("Australia/Adelaide", "ACST"),
            ("Australia/Brisbane", "AEST"), ("Australia/Broken_Hill", "ACST"), ("Australia/Canberra", "AEST"), ("Australia/Currie", "AEST"),
            ("Australia/Darwin", "ACST"), ("Australia/Eucla", "ACWST"), ("Australia/Hobart", "AEST"), ("Australia/LHI", "LHST"),
            ("Australia/Lindeman", "AEST"), ("Australia/Lord_Howe", "LHST"), ("Australia/Melbourne", "AEST"), ("Australia/North", "ACST"),
            ("Australia/NSW", "AEST"), ("Australia/Perth", "AWST"), ("Australia/Queensland", "AEST"), ("Australia/South", "ACST"),
            ("Australia/Sydney", "AEST"), ("Australia/Tasmania", "AEST"), ("Australia/Victoria", "AEST"), ("Australia/West", "AWST"),
            ("Australia/Yancowinna", "ACST"), ("Europe/Andorra", "CET"), ("Europe/Astrakhan", "SAMT"), ("Europe/Athens", "EET"),
            ("Europe/Belgrade", "CET"), ("Europe/Berlin", "CET"), ("Europe/Brussels", "CET"), ("Europe/Bucharest", "EET"),
            ("Europe/Budapest", "CET"), ("Europe/Chisinau", "EET"), ("Europe/Dublin", "GMT"), ("Europe/Gibraltar", "CET"),
            ("Europe/Helsinki", "EET"), ("Europe/Istanbul", "TRT"), ("Europe/Kaliningrad", "EET"), ("Europe/Kirov", "MSK"),
            ("Europe/Kyiv", "EET"), ("Europe/Lisbon", "WET"), ("Europe/London", "GMT"), ("Europe/Madrid", "CET"),
            ("Europe/Malta", "CET"), ("Europe/Minsk", "MSK"), ("Europe/Moscow", "MSK"), ("Europe/Paris", "CET"),
            ("Europe/Prague", "CET"), ("Europe/Riga", "EET"), ("Europe/Rome", "CET"), ("Europe/Samara", "SAMT"),
            ("Europe/Saratov", "SAMT"), ("Europe/Simferopol", "MSK"), ("Europe/Sofia", "EET"), ("Europe/Tallinn", "EET"),
            ("Europe/Tirane", "CET"), ("Europe/Ulyanovsk", "SAMT"), ("Europe/Vienna", "CET"), ("Europe/Vilnius", "EET"),
            ("Europe/Volgograd", "MSK"), ("Europe/Warsaw", "CET"), ("Europe/Zurich", "CET"), ("Indian/Chagos", "IOT"),
            ("Indian/Christmas", "CXT"), ("Indian/Cocos", "CCT"), ("Indian/Kerguelen", "TFT"), ("Indian/Maldives", "MVT"),
            ("Indian/Mauritius", "MUT"), ("Indian/Mayotte", "EAT"), ("Indian/Reunion", "RET"), ("Pacific/Apia", "WST"),
            ("Pacific/Auckland", "NZST"), ("Pacific/Bougainville", "BST"), ("Pacific/Chatham", "CHAST"), ("Pacific/Easter", "EASST"),
            ("Pacific/Efate", "VUT"), ("Pacific/Fakaofo", "TKT"), ("Pacific/Fiji", "FJT"), ("Pacific/Galapagos", "GALT"),
            ("Pacific/Gambier", "GAMT"), ("Pacific/Guadalcanal", "SBT"), ("Pacific/Guam", "ChST"), ("Pacific/Honolulu", "HST"),
            ("Pacific/Kanton", "PHOT"), ("Pacific/Kiritimati", "LINT"), ("Pacific/Kosrae", "KOST"), ("Pacific/Kwajalein", "MHT"),
            ("Pacific/Marquesas", "MART"), ("Pacific/Nauru", "NRT"), ("Pacific/Niue", "NUT"), ("Pacific/Norfolk", "NFT"),
            ("Pacific/Noumea", "NCT"), ("Pacific/Pago_Pago", "SST"), ("Pacific/Palau", "PWT"), ("Pacific/Pitcairn", "PST"),
            ("Pacific/Port_Moresby", "PGT"), ("Pacific/Rarotonga", "CKT"), ("Pacific/Tahiti", "TAHT"), ("Pacific/Tarawa", "GILT"),
            ("Pacific/Tongatapu", "TOT"), ("Pacific/Wake", "WAKT"), ("Pacific/Wallis", "WFT"), ("UTC", "UTC"),
            ("Etc/UTC", "UTC"), ("Etc/GMT", "GMT"),
        ])
    })
}

fn offset_abbreviations() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            ("+00:00", "GMT"), ("+01:00", "CET"), ("+02:00", "EET"), ("+03:00", "MSK"), ("+04:00", "GST"),
            ("+05:00", "PKT"), ("+05:30", "IST"), ("+05:45", "NPT"), ("+06:00", "BST"), ("+06:30", "MMT"),
            ("+07:00", "ICT"), ("+08:00", "CST"), ("+08:45", "ACWST"), ("+09:00", "JST"), ("+09:30", "ACST"),
            ("+10:00", "AEST"), ("+10:30", "LHST"), ("+11:00", "AEDT"), ("+12:00", "NZST"), ("+13:00", "NZDT"),
            ("+14:00", "LINT"), ("-01:00", "AZOT"), ("-02:00", "GST"), ("-03:00", "ART"), ("-03:30", "NST"),
            ("-04:00", "AST"), ("-05:00", "EST"), ("-06:00", "CST"), ("-07:00", "MST"), ("-08:00", "PST"),
            ("-09:00", "AKST"), ("-10:00", "HST"), ("-11:00", "SST"), ("-12:00", "AoE"),
        ])
    })
}

fn seasonal_abbreviations() -> &'static HashMap<&'static str, [&'static str; 2]> {
    static MAP: OnceLock<HashMap<&'static str, [&'static str; 2]>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            ("America/Anchorage", ["AKST", "AKDT"]), ("America/Chicago", ["CST", "CDT"]),
            ("America/Denver", ["MST", "MDT"]), ("America/Detroit", ["EST", "EDT"]),
            ("America/Halifax", ["AST", "ADT"]), ("America/Los_Angeles", ["PST", "PDT"]),
            ("America/New_York", ["EST", "EDT"]), ("America/Santiago", ["CLT", "CLST"]),
            ("America/St_Johns", ["NST", "NDT"]), ("America/Toronto", ["EST", "EDT"]),
            ("America/Vancouver", ["PST", "PDT"]), ("America/Winnipeg", ["CST", "CDT"]),
            ("Atlantic/Azores", ["AZOT", "AZOST"]), ("Atlantic/Bermuda", ["AST", "ADT"]),
            ("Australia/Adelaide", ["ACST", "ACDT"]), ("Australia/Hobart", ["AEST", "AEDT"]),
            ("Australia/Melbourne", ["AEST", "AEDT"]), ("Australia/Sydney", ["AEST", "AEDT"]),
            ("Europe/Athens", ["EET", "EEST"]), ("Europe/Berlin", ["CET", "CEST"]),
            ("Europe/Brussels", ["CET", "CEST"]), ("Europe/Dublin", ["GMT", "IST"]),
            ("Europe/Helsinki", ["EET", "EEST"]), ("Europe/Lisbon", ["WET", "WEST"]),
            ("Europe/London", ["GMT", "BST"]), ("Europe/Madrid", ["CET", "CEST"]),
            ("Europe/Paris", ["CET", "CEST"]), ("Europe/Prague", ["CET", "CEST"]),
            ("Europe/Rome", ["CET", "CEST"]), ("Europe/Vienna", ["CET", "CEST"]),
            ("Europe/Warsaw", ["CET", "CEST"]), ("Europe/Zurich", ["CET", "CEST"]),
            ("Pacific/Auckland", ["NZST", "NZDT"]), ("Pacific/Chatham", ["CHAST", "CHADT"]),
        ])
    })
}

fn split_offset(offset: i32) -> (&'static str, i32, i32) {
    let (sign, abs) = if offset < 0 { ("-", -offset) } else { ("+", offset) };
    (sign, abs / 3600, (abs % 3600) / 60)
}

fn offset_key(offset: i32) -> String {
    let (sign, h, m) = split_offset(offset);
    format!("{sign}{h:02}:{m:02}")
}

fn zone_abbreviation(zone: &str, tz: Tz, now: DateTime<Utc>) -> String {
    let local = now.with_timezone(&tz);
    let offset = local.offset().fix().local_minus_utc();
    let name = local.format("%Z").to_string();
    if !name.is_empty() && !name.starts_with('+') && !name.starts_with('-') {
        return name;
    }
    if let Some(pair) = seasonal_abbreviations().get(zone) {
        let year = now.year();
        let jan = tz.with_ymd_and_hms(year, 1, 1, 12, 0, 0).single()
            .map(|d| d.offset().fix().local_minus_utc()).unwrap_or(offset);
        let jul = tz.with_ymd_and_hms(year, 7, 1, 12, 0, 0).single()
            .map(|d| d.offset().fix().local_minus_utc()).unwrap_or(offset);
        if jan != jul {
            return if offset == jan.max(jul) { pair[1] } else { pair[0] }.to_owned();
        }
    }
    if let Some(&exact) = zone_abbreviations().get(zone) {
        return exact.to_owned();
    }
    let key = offset_key(offset);
    if let Some(&fb) = offset_abbreviations().get(key.as_str()) {
        return fb.to_owned();
    }
    format!("GMT{key}")
}

fn zone_label(zone: &str, format: &str, now: DateTime<Utc>) -> String {
    let Ok(tz) = zone.parse::<Tz>() else {
        return String::new();
    };
    if format.len() >= 7 && format[..7].eq_ignore_ascii_case("custom:") {
        return format[7..].to_owned();
    }
    let offset = now.with_timezone(&tz).offset().fix().local_minus_utc();
    let (sign, hours, minutes) = split_offset(offset);
    match format.trim().to_ascii_uppercase().as_str() {
        "SIMP" => zone_abbreviation(zone, tz, now),
        "OFFSET" => offset_key(offset),
        "UTC" => {
            if hours == 0 && minutes == 0 {
                "UTC".into()
            } else if minutes != 0 {
                format!("UTC{}", offset_key(offset))
            } else {
                format!("UTC{sign}{hours}")
            }
        }
        _ => {
            if hours == 0 && minutes == 0 {
                "GMT".into()
            } else if minutes != 0 {
                format!("GMT{}", offset_key(offset))
            } else {
                format!("GMT{sign}{hours}")
            }
        }
    }
}

fn apply_text_style(text: &str, style: &str) -> String {
    let ranges: [u32; 3] = match style.trim().to_ascii_lowercase().as_str() {
        "italic" => [0x1D7CE, 0x1D400, 0x1D41A],
        "double" => [0x1D7D8, 0, 0x1D552],
        "sans" => [0x1D7EC, 0x1D5D4, 0x1D5EE],
        "mono" => [0x1D7F6, 0x1D670, 0x1D68A],
        "outline" => [0x1D7E2, 0x1D5A0, 0x1D5BA],
        _ => return text.to_owned(),
    };
    if text.is_empty() {
        return String::new();
    }
    let double_upper = |c: char| -> Option<char> {
        match c {
            'C' => Some('ℂ'), 'H' => Some('ℍ'), 'N' => Some('ℕ'), 'P' => Some('ℙ'),
            'Q' => Some('ℚ'), 'R' => Some('ℝ'), 'Z' => Some('ℤ'),
            _ => None,
        }
    };
    let lower_style = style.trim().to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    for r in text.chars() {
        match r {
            '0'..='9' => out.push(char::from_u32(ranges[0] + (r as u32 - '0' as u32)).unwrap_or(r)),
            'A'..='Z' => {
                if lower_style == "double" {
                    if let Some(m) = double_upper(r) {
                        out.push(m);
                    } else {
                        out.push(char::from_u32(0x1D538 + (r as u32 - 'A' as u32)).unwrap_or(r));
                    }
                } else {
                    out.push(char::from_u32(ranges[1] + (r as u32 - 'A' as u32)).unwrap_or(r));
                }
            }
            'a'..='z' => out.push(char::from_u32(ranges[2] + (r as u32 - 'a' as u32)).unwrap_or(r)),
            _ => out.push(r),
        }
    }
    out
}

fn compose_name(user: &AcnUser, texts: &[String], weather: &str, now: DateTime<Utc>) -> String {
    let selected: HashSet<&str> = user
        .display_components
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(String::as_str)
        .collect();
    let includes = |comp: &str| selected.is_empty() || selected.contains(comp);
    let mut parts: HashMap<&str, String> = HashMap::new();
    parts.insert("name", user.original_first_name.clone());
    parts.insert("weather", weather.to_owned());
    if (user.mode == "text" || user.mode == "both" || selected.contains("text"))
        && includes("text")
        && !texts.is_empty()
    {
        let idx = user.text_index.rem_euclid(texts.len() as isize) as usize;
        parts.insert("text", texts[idx].clone());
    }
    if user.show_time() && includes("time") {
        parts.insert("time", zone_time(&user.timezone, &user.hour_format, now));
    }
    if user.show_clock_emoji {
        parts.insert("emoji", clock_emoji(&user.timezone, now));
    }
    if user.show_timezone {
        parts.insert("timezone", zone_label(&user.timezone, &user.timezone_format, now));
    }

    let order = if user.display_order.trim().is_empty() {
        ACN_DEFAULT_ORDER
    } else {
        user.display_order.as_str()
    };
    let mut pieces = Vec::new();
    let mut seen = HashSet::new();
    for key in order.split(',').chain(ACN_COMPONENTS.iter().copied()) {
        let key = key.trim();
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        let Some(val) = parts.get(key) else { continue };
        if val.is_empty() {
            continue;
        }
        if key == "name" {
            pieces.push(val.clone());
        } else {
            pieces.push(apply_text_style(val, &user.text_style));
        }
    }
    pieces.join(" ")
}

fn with_component(order: &str, component: &str, enabled: bool) -> String {
    let order = if order.trim().is_empty() { ACN_DEFAULT_ORDER } else { order };
    let mut parts: Vec<&str> = Vec::new();
    let mut present = false;
    for p in order.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if p == component {
            present = true;
            if !enabled {
                continue;
            }
        }
        parts.push(p);
    }
    if enabled && !present {
        parts.push(component);
    }
    parts.join(",")
}

fn apply_order(user: &mut AcnUser, components: &[String]) {
    user.display_order = components.join(",");
    user.show_time = Some(components.iter().any(|c| c == "time"));
    user.show_clock_emoji = components.iter().any(|c| c == "emoji");
    user.show_timezone = components.iter().any(|c| c == "timezone");
    if components.iter().any(|c| c == "weather") {
        user.weather_enabled = true;
    }
}

fn toggle_shown(user: &mut AcnUser, component: &str, on: bool) {
    let mut current = user
        .display_components
        .clone()
        .unwrap_or_else(|| show_defaults(&user.mode));
    if on {
        if !current.iter().any(|c| c == component) {
            current.push(component.to_owned());
        }
    } else {
        current.retain(|c| c != component);
    }
    user.display_components = Some(current);
    if component == "weather" {
        user.weather_enabled = on;
    }
}

fn acn_cities() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            ("北京", "Beijing"), ("上海", "Shanghai"), ("广州", "Guangzhou"), ("深圳", "Shenzhen"),
            ("成都", "Chengdu"), ("杭州", "Hangzhou"), ("武汉", "Wuhan"), ("西安", "Xi'an"), ("重庆", "Chongqing"), ("南京", "Nanjing"),
            ("天津", "Tianjin"), ("苏州", "Suzhou"), ("长沙", "Changsha"), ("郑州", "Zhengzhou"), ("青岛", "Qingdao"), ("大连", "Dalian"),
            ("厦门", "Xiamen"), ("香港", "Hong Kong"), ("澳门", "Macau"), ("台北", "Taipei"), ("东京", "Tokyo"), ("大阪", "Osaka"),
            ("京都", "Kyoto"), ("首尔", "Seoul"), ("曼谷", "Bangkok"), ("新加坡", "Singapore"), ("吉隆坡", "Kuala Lumpur"),
            ("雅加达", "Jakarta"), ("伦敦", "London"), ("巴黎", "Paris"), ("柏林", "Berlin"), ("罗马", "Rome"), ("纽约", "New York"),
            ("洛杉矶", "Los Angeles"), ("旧金山", "San Francisco"), ("芝加哥", "Chicago"), ("多伦多", "Toronto"), ("悉尼", "Sydney"), ("墨尔本", "Melbourne"),
        ])
    })
}

fn weather_icon(code: i32) -> &'static str {
    match code {
        0 => "☀️",
        1 => "🌤️",
        2 => "⛅",
        3 => "☁️",
        45 | 48 => "🌫️",
        51 | 53 | 80 => "🌦️",
        55 | 61 | 63 | 65 | 81 => "🌧️",
        56 | 57 | 66 | 67 | 77 | 85 | 86 => "🌨️",
        71 | 73 | 75 => "❄️",
        82 | 95 | 96 | 99 => "⛈️",
        _ => "🌤️",
    }
}

fn round_half_up(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 { floor + 1.0 } else { floor }
}

fn format_weather(code: i32, temperature: f64) -> String {
    let rounded = round_half_up(temperature) as i64;
    format!("{} {rounded}°C", weather_icon(code))
}

async fn fetch_weather(location: &str) -> String {
    let trimmed = location.trim();
    let city = acn_cities().get(trimmed).copied().unwrap_or(trimmed);
    #[derive(Deserialize)]
    struct GeoResult {
        latitude: f64,
        longitude: f64,
    }
    #[derive(Deserialize)]
    struct GeoResp {
        #[serde(default)]
        results: Vec<GeoResult>,
    }
    let Ok(mut geo_url) = url::Url::parse("https://geocoding-api.open-meteo.com/v1/search?count=1&language=zh&format=json") else {
        return String::new();
    };
    geo_url.query_pairs_mut().append_pair("name", city);
    let Ok(geo) = get_json::<GeoResp>(geo_url.as_str(), Some(Duration::from_secs(10)), Some(256 << 10)).await else {
        return String::new();
    };
    let Some(first) = geo.results.first() else {
        return String::new();
    };
    #[derive(Deserialize)]
    struct ForecastCurrent {
        temperature_2m: Option<f64>,
        weather_code: Option<i32>,
    }
    #[derive(Deserialize)]
    struct ForecastResp {
        current: ForecastCurrent,
    }
    let endpoint = format!(
        "https://api.open-meteo.com/v1/forecast?current=temperature_2m,weather_code&timezone=auto&latitude={:.4}&longitude={:.4}",
        first.latitude, first.longitude
    );
    let Ok(forecast) = get_json::<ForecastResp>(&endpoint, Some(Duration::from_secs(10)), Some(256 << 10)).await else {
        return String::new();
    };
    match (forecast.current.weather_code, forecast.current.temperature_2m) {
        (Some(code), Some(temp)) => format_weather(code, temp),
        _ => String::new(),
    }
}

fn weather_cache_fresh(user: &AcnUser, now_ms: i64) -> bool {
    if user.weather_cache_ts <= 0 {
        return false;
    }
    let ttl_ms = if user.weather_compact.is_empty() { 5 * 60_000 } else { 30 * 60_000 };
    let diff = now_ms - user.weather_cache_ts;
    diff < ttl_ms
}

async fn current_weather(
    cancel: &CancellationToken,
    user: &AcnUser,
    include_disabled: bool,
    now_ms: i64,
) -> (String, bool) {
    if (!include_disabled && !user.weather_enabled) || user.weather_location.is_empty() {
        return (String::new(), false);
    }
    if weather_cache_fresh(user, now_ms) {
        return (user.weather_compact.clone(), false);
    }
    let text = fetch_weather(&user.weather_location).await;
    (text, !cancel.is_cancelled())
}

struct AcnService {
    store: Arc<Store<AcnDoc>>,
    logs: Option<Arc<mibot_logtail::Ring>>,
}

fn clock_time_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b\d{1,2}:\d{2}(\s?(AM|PM))?\b").expect("valid regex"))
}

fn clean_nickname(name: &str) -> String {
    let trunc = kit::truncate_runes(name, 128);
    let mut b = String::with_capacity(trunc.len());
    for ch in trunc.chars() {
        let cp = ch as u32;
        if !(0x1F550..=0x1F567).contains(&cp) {
            b.push(ch);
        }
    }
    let cleaned = clock_time_re().replace_all(&b, "");
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl AcnService {
    fn normalize_stored(&self, app: &App) {
        if let Err(err) = kit::update_ok(&self.store, |doc| {
            doc.schema_version = 1;
            let keys: Vec<String> = doc.users.keys().cloned().collect();
            for id in keys {
                if let Some(u) = doc.users.get_mut(&id) {
                    normalize_user(&id, u);
                }
            }
            if doc.random_texts.len() > 100 {
                doc.random_texts.truncate(100);
            }
        }) {
            kit::warn(app, "acn.normalize_failed", &err);
        }
    }

    async fn apply(&self, cancel: &CancellationToken, client: &Client, user_id: &str, force: bool) -> Result<bool, CmdError> {
        let state = self.store.snapshot()?;
        let Some(user) = state.users.get(user_id).cloned() else {
            return Ok(false);
        };
        if user.original_first_name.is_empty() || (!force && !user.enabled) {
            return Ok(false);
        }
        if !force && !user.last_update.is_empty()
            && let Ok(when) = DateTime::parse_from_rfc3339(&user.last_update)
            && (Utc::now() - when.with_timezone(&Utc)) < chrono::Duration::seconds(30)
        {
            return Ok(false);
        }
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let (weather, fetched) = current_weather(cancel, &user, false, now_ms).await;
        if fetched {
            let latest = self.store.snapshot()?;
            if !Arc::ptr_eq(&latest, &state) {
                return Ok(false);
            }
        }
        let first_name = kit::truncate_runes(&compose_name(&user, &state.random_texts, &weather, now), 64);
        let req = tl::functions::account::UpdateProfile {
            first_name: Some(first_name),
            last_name: (!user.original_last_name.is_empty()).then(|| user.original_last_name.clone()),
            about: None,
        };
        if let Err(err) = client.invoke(&req).await {
            if err.is_rpc("USERNAME_NOT_MODIFIED") {
                return Ok(true);
            }
            if err.flood_wait().is_some() {
                if let Err(save_err) = kit::update_ok(&self.store, |doc| {
                    if let Some(cur) = doc.users.get_mut(user_id) {
                        cur.enabled = false;
                    }
                }) {
                    kit::opt_log(
                        self.logs.as_ref(),
                        mibot_logtail::Level::Error,
                        "acn.flood_disable_failed",
                        &[("user", user_id), ("error", &save_err.to_string())],
                    );
                } else {
                    kit::opt_log(
                        self.logs.as_ref(),
                        mibot_logtail::Level::Warn,
                        "acn.flood_disabled",
                        &[("user", user_id)],
                    );
                }
            }
            return Err(Box::new(err));
        }
        if user.enabled {
            let latest = self.store.snapshot()?;
            if let Some(cur) = latest.users.get(user_id)
                && !cur.enabled
            {
                self.restore(client, cur).await?;
                return Ok(false);
            }
        }
        kit::update_ok(&self.store, |doc| {
            let text_count = doc.random_texts.len();
            if let Some(cur) = doc.users.get_mut(user_id) {
                cur.last_update = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                if fetched && cur.weather_location == user.weather_location {
                    cur.weather_compact = weather;
                    cur.weather_cache_ts = now_ms;
                }
                if text_count > 0 && cur.mode != "time" {
                    cur.text_index = (cur.text_index + 1) % (text_count as isize);
                }
            }
        })?;
        Ok(true)
    }

    async fn restore(&self, client: &Client, user: &AcnUser) -> Result<(), mibot_bot::BotError> {
        let req = tl::functions::account::UpdateProfile {
            first_name: Some(user.original_first_name.clone()),
            last_name: Some(user.original_last_name.clone()),
            about: None,
        };
        match client.invoke(&req).await {
            Ok(_) => Ok(()),
            Err(err) if err.is_rpc("USERNAME_NOT_MODIFIED") => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn tick(&self, cancel: &CancellationToken, client: &Client) {
        let id = client.self_id().to_string();
        if let Err(err) = self.apply(cancel, client, &id, false).await
            && !cancel.is_cancelled()
        {
            kit::opt_log(
                self.logs.as_ref(),
                mibot_logtail::Level::Warn,
                "acn.update_failed",
                &[("user", &id), ("error", &err.to_string())],
            );
        }
    }
}

fn hour_format_of(u: &AcnUser) -> &'static str {
    if u.hour_format == "12" { "12" } else { "24" }
}

async fn current_self(client: &Client) -> Result<tl::types::User, CmdError> {
    let users = client
        .invoke(&tl::functions::users::GetUsers {
            id: vec![tl::enums::InputUser::UserSelf],
        })
        .await?;
    for entry in users {
        if let tl::enums::User::User(u) = entry {
            return Ok(u);
        }
    }
    Err(kit::fail("读不到账号自己的资料"))
}

struct AcnCall<'a> {
    inv: &'a Invocation,
    service: &'a AcnService,
    doc: AcnDoc,
    user_id: String,
    user: AcnUser,
    sub: String,
}

impl AcnCall<'_> {
    fn mutate<F: FnOnce(&mut AcnUser)>(&self, apply: F) -> Result<AcnUser, CmdError> {
        let mut updated = AcnUser::default();
        let uid = self.user_id.clone();
        kit::update_ok(&self.service.store, |doc| {
            if let Some(cur) = doc.users.get_mut(&uid) {
                apply(cur);
                updated = cur.clone();
            }
        })?;
        Ok(updated)
    }

    async fn change<F: FnOnce(&mut AcnUser)>(&self, apply: F) -> Result<AcnUser, CmdError> {
        let updated = self.mutate(apply)?;
        if !updated.enabled {
            return Ok(updated);
        }
        if let Err(err) = self.service.apply(&self.inv.cancel, &self.inv.client, &self.user_id, true).await
            && !self.inv.cancel.is_cancelled()
        {
            kit::opt_log(
                self.service.logs.as_ref(),
                mibot_logtail::Level::Warn,
                "acn.refresh_failed",
                &[("error", &err.to_string())],
            );
        }
        Ok(updated)
    }

    async fn toggle(&self) -> CmdResult {
        let enabled = self.sub == "on" || self.sub == "enable";
        self.mutate(|u| u.enabled = enabled)?;
        if enabled {
            self.service
                .apply(&self.inv.cancel, &self.inv.client, &self.user_id, true)
                .await
                .map_err(|e| kit::fail_with("开启后首次更新失败", mibot_command::UserError::new(e.to_string())))?;
            self.inv.edit_text("✅ 动态昵称已开启").await?;
        } else {
            self.service
                .restore(&self.inv.client, &self.user)
                .await
                .map_err(|e| kit::fail_with("恢复原始昵称失败", e))?;
            self.inv.edit_text("✅ 动态昵称已关闭").await?;
        }
        Ok(())
    }

    async fn mode(&self) -> CmdResult {
        let next = match self.user.mode.as_str() {
            "time" => "text",
            "text" => "both",
            _ => "time",
        };
        self.change(|u| u.mode = next.into()).await?;
        self.inv.edit(&format!("✅ 显示模式已设为 {}", code(next))).await?;
        Ok(())
    }

    async fn timezone(&self) -> CmdResult {
        let value = self.inv.arg(1).to_ascii_lowercase();
        match value.as_str() {
            "" => {
                let p = escape(self.inv.prefix());
                self.inv.edit(&format!(
                    "🌍 <b>时区管理</b>\n\n• <code>{p}acn tz Asia/Shanghai</code> 设置时区\n• <code>{p}acn tz list</code> 时区列表\n• <code>{p}acn tz on|off</code> 显示控制\n• <code>{p}acn tz format GMT</code> 格式设置"
                )).await?;
                return Ok(());
            }
            "list" => {
                self.inv.edit_text("Asia/Shanghai\nAsia/Tokyo\nEurope/London\nAmerica/New_York").await?;
                return Ok(());
            }
            "on" | "off" => {
                let on = value == "on";
                self.change(|u| {
                    u.show_timezone = on;
                    u.display_order = with_component(&u.display_order, "timezone", on);
                })
                .await?;
                self.inv.edit_text(&format!("✅ 时区显示已{}", kit::on_off_text(on))).await?;
                return Ok(());
            }
            "format" => return self.timezone_format().await,
            _ => {}
        }
        let zone = if value == "set" { self.inv.raw_after(2) } else { self.inv.raw_after(1) }.trim().to_owned();
        if !valid_zone(&zone) {
            return Err(kit::fail("无效的时区标识符"));
        }
        self.change(|u| u.timezone = zone.clone()).await?;
        self.inv.edit(&format!("✅ 时区已更新为：{}", code(&zone))).await?;
        Ok(())
    }

    async fn timezone_format(&self) -> CmdResult {
        let fmt_str = self.inv.raw_after(2).trim().to_owned();
        if fmt_str.is_empty() {
            self.inv.edit(&format!(
                "🌐 <b>时区格式设置</b>\n当前：{}\n可用：GMT, UTC, simp, offset, custom:文字",
                code(kit::or_default(&self.user.timezone_format, "GMT"))
            )).await?;
            return Ok(());
        }
        let lower = fmt_str.to_ascii_lowercase();
        let custom = lower.starts_with("custom:") && fmt_str.len() > 7;
        if !custom && !matches!(lower.as_str(), "gmt" | "utc" | "simp" | "offset") {
            return Err(kit::fail("无效的时区格式"));
        }
        self.change(|u| {
            u.timezone_format = if custom {
                format!("custom:{}", &fmt_str[7..])
            } else {
                fmt_str.to_ascii_uppercase()
            };
        })
        .await?;
        self.inv.edit_text("✅ 时区格式已更新").await?;
        Ok(())
    }

    async fn flag(&self) -> CmdResult {
        let arg = self.inv.arg(1).to_ascii_lowercase();
        let is_emoji = self.sub == "emoji";
        let (label, usage, current) = if is_emoji {
            ("时钟表情", format!("用法：{}acn emoji on|off", self.inv.prefix()), self.user.show_clock_emoji)
        } else {
            ("时间显示", format!("用法：{}acn time on|off 或 {}acn time 12|24", self.inv.prefix(), self.inv.prefix()), self.user.show_time())
        };
        if arg.is_empty() {
            let mut text = format!("🏷 <b>{label}</b>\n当前：{}", code(kit::on_off_text(current)));
            if !is_emoji {
                text.push_str(&format!("\n时间制式：{}", code(&format!("{} 小时制", hour_format_of(&self.user)))));
            }
            text.push_str(&format!("\n{}", escape(&usage)));
            self.inv.edit(&text).await?;
            return Ok(());
        }
        if !is_emoji {
            let raw_fmt = if arg == "format" {
                self.inv.arg(2).to_ascii_lowercase()
            } else {
                arg.clone()
            };
            let fmt = raw_fmt.strip_suffix('h').unwrap_or(&raw_fmt);
            if fmt == "12" || fmt == "24" {
                self.change(|u| u.hour_format = fmt.to_owned()).await?;
                let ex = if fmt == "12" { "（如 02:32 PM）" } else { "（如 14:32）" };
                self.inv.edit_text(&format!("✅ 已切换为 {fmt} 小时制{ex}")).await?;
                return Ok(());
            }
        }
        let Ok(enabled) = kit::on_off(&arg) else {
            self.inv.edit_text(&usage).await?;
            return Ok(());
        };
        self.change(|u| {
            if is_emoji {
                u.show_clock_emoji = enabled;
                u.display_order = with_component(&u.display_order, "emoji", enabled);
            } else {
                u.show_time = Some(enabled);
                u.display_order = with_component(&u.display_order, "time", enabled);
            }
        })
        .await?;
        self.inv.edit_text(&format!("✅ {label}已{}", kit::on_off_text(enabled))).await?;
        Ok(())
    }

    async fn style(&self) -> CmdResult {
        let s = self.inv.arg(1).to_ascii_lowercase();
        if !ACN_STYLES.contains(&s.as_str()) {
            self.inv.edit_text(&format!("可用样式：{}", ACN_STYLES.join(", "))).await?;
            return Ok(());
        }
        self.change(|u| u.text_style = s.clone()).await?;
        self.inv.edit_text(&format!("✅ 文字样式已设为 {s}")).await?;
        Ok(())
    }

    async fn order(&self) -> CmdResult {
        let rest = self.inv.raw_after(1).to_ascii_lowercase();
        let values: Vec<&str> = rest
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .collect();
        if values.is_empty() {
            self.inv.edit(&format!("当前顺序：{}", code(kit::or_default(&self.user.display_order, ACN_DEFAULT_ORDER)))).await?;
            return Ok(());
        }
        let mut invalid = Vec::new();
        let mut unique: Vec<String> = Vec::new();
        for v in values {
            if !ACN_COMPONENTS.contains(&v) {
                invalid.push(v);
            } else if !unique.iter().any(|x| x == v) {
                unique.push(v.to_owned());
            }
        }
        if !invalid.is_empty() {
            return Err(kit::fail(format!(
                "无效组件：{}。可用的有：{}",
                invalid.join("、"),
                ACN_COMPONENTS.join("、")
            )));
        }
        let joined = unique.join(",");
        self.change(|u| apply_order(u, &unique)).await?;
        self.inv.edit(&format!("✅ 显示顺序已设为 {}", code(&joined))).await?;
        Ok(())
    }

    async fn show(&self) -> CmdResult {
        let action = self.inv.arg(1).to_ascii_lowercase();
        let target = self.inv.arg(2).to_ascii_lowercase();
        let p = escape(self.inv.prefix());
        if action.is_empty() || action == "help" || action == "h" {
            let cur = self
                .user
                .display_components
                .clone()
                .unwrap_or_else(|| show_defaults(&self.user.mode));
            self.inv.edit(&format!(
                "🎛️ <b>显示组件管理</b>\n\n当前组件：{}\n\n• <code>{p}acn show time on|off</code>\n• <code>{p}acn show text on|off</code>\n• <code>{p}acn show weather on|off</code>\n• <code>{p}acn show reset</code>",
                code(&cur.join(", "))
            )).await?;
            return Ok(());
        }
        if action == "reset" {
            let updated = self
                .change(|u| u.display_components = Some(show_defaults(&u.mode)))
                .await?;
            let list = updated.display_components.unwrap_or_default().join(", ");
            self.inv.edit(&format!("✅ <b>已重置为默认值</b>\n\n当前模式默认组件：{}", code(&list))).await?;
            return Ok(());
        }
        if !matches!(action.as_str(), "time" | "text" | "weather") {
            return Err(kit::fail("acn show 只能管理 time、text、weather"));
        }
        if target != "on" && target != "off" {
            return Err(kit::fail(format!("请指定 on 或 off，用法：{}acn show {action} on|off", self.inv.prefix())));
        }
        if action == "weather" && target == "on" && self.user.weather_location.trim().is_empty() {
            return Err(kit::fail(format!("请先设置天气地点，用法：{}acn weather set 北京", self.inv.prefix())));
        }
        let on = target == "on";
        let updated = self.change(|u| toggle_shown(u, &action, on)).await?;
        let list = updated.display_components.unwrap_or_default().join(", ");
        self.inv.edit(&format!(
            "✅ <b>组件已{}</b>\n当前组件：{}",
            kit::on_off_text(on),
            code(&list)
        )).await?;
        Ok(())
    }

    async fn config(&self) -> CmdResult {
        let u = &self.user;
        let count = self.doc.random_texts.len();
        let next_text = if count > 0 {
            ((u.text_index.rem_euclid(count as isize)) + 1).to_string()
        } else {
            "无".into()
        };
        let weather_time = if u.weather_cache_ts > 0 {
            DateTime::<Utc>::from_timestamp_millis(u.weather_cache_ts)
                .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_else(|| "尚未获取".into())
        } else {
            "尚未获取".into()
        };
        let rows = [
            ("用户", u.user_id.as_str()),
            ("自动更新", kit::on_off_text(u.enabled)),
            ("原始姓名", u.original_first_name.as_str()),
            ("原始姓氏", kit::or_default(&u.original_last_name, "无")),
            ("模式", u.mode.as_str()),
            ("时区", u.timezone.as_str()),
            ("时间显示", kit::on_off_text(u.show_time())),
            ("时间制式", if u.hour_format == "12" { "12 小时制" } else { "24 小时制" }),
            ("时钟表情", kit::on_off_text(u.show_clock_emoji)),
            ("时区显示", kit::on_off_text(u.show_timezone)),
            ("时区格式", kit::or_default(&u.timezone_format, "GMT")),
            ("文字样式", kit::or_default(&u.text_style, "normal")),
            ("组件顺序", kit::or_default(&u.display_order, ACN_DEFAULT_ORDER)),
            ("文案数", &count.to_string()),
            ("下条文案序号", &next_text),
            ("天气显示", kit::on_off_text(u.weather_enabled)),
            ("天气地点", kit::or_default(&u.weather_location, "未设置")),
            ("天气预览", kit::or_default(&u.weather_compact, "暂无缓存")),
            ("天气更新时间", &weather_time),
            ("昵称更新时间", kit::or_default(&u.last_update, "尚未更新")),
        ];
        let mut lines = vec!["🔧 <b>你的配置</b>".to_owned()];
        for (k, v) in rows {
            lines.push(format!("{}：{}", escape(k), code(v)));
        }
        self.inv.edit(&lines.join("\n")).await?;
        Ok(())
    }

    async fn update_now(&self) -> CmdResult {
        let ok = self
            .service
            .apply(&self.inv.cancel, &self.inv.client, &self.user_id, true)
            .await
            .map_err(|e| kit::fail_with("更新失败", mibot_command::UserError::new(e.to_string())))?;
        if !ok {
            return Err(kit::fail("更新失败"));
        }
        self.inv.edit_text("✅ 昵称已手动更新").await?;
        Ok(())
    }

    async fn reset(&self) -> CmdResult {
        self.mutate(|u| u.enabled = false)?;
        self.service
            .restore(&self.inv.client, &self.user)
            .await
            .map_err(|e| kit::fail_with("恢复原始昵称失败", e))?;
        self.inv.edit_text("✅ 已恢复原始昵称并关闭自动更新").await?;
        Ok(())
    }

    async fn text(&self) -> CmdResult {
        let action = self.inv.arg(1).to_ascii_lowercase();
        match action.as_str() {
            "list" => {
                if self.doc.random_texts.is_empty() {
                    return self.inv.edit_text("还没有随机文本").await.map_err(Into::into);
                }
                let lines: Vec<String> = self
                    .doc
                    .random_texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| format!("{}. {}", i + 1, escape(t)))
                    .collect();
                self.inv.edit(&lines.join("\n")).await?;
                Ok(())
            }
            "clear" => {
                kit::update_ok(&self.service.store, |d| d.random_texts.clear())?;
                self.inv.edit_text("✅ 所有文本已清空").await?;
                Ok(())
            }
            "add" => {
                let body = self.inv.raw_after(2);
                let additions: Vec<String> = body
                    .split('\n')
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && l.chars().count() <= 50)
                    .map(str::to_owned)
                    .collect();
                if additions.is_empty() {
                    return Err(kit::fail("没有可添加的文案（每条最长 50 字符）"));
                }
                let mut added = 0usize;
                kit::update_ok(&self.service.store, |d| {
                    let mut existing: HashSet<String> = d.random_texts.iter().cloned().collect();
                    for t in additions {
                        if d.random_texts.len() >= 100 || !existing.insert(t.clone()) {
                            continue;
                        }
                        d.random_texts.push(t);
                        added += 1;
                    }
                })?;
                self.inv.edit_text(&format!("✅ 已添加 {added} 条")).await?;
                Ok(())
            }
            "del" => {
                let s = self.inv.arg(2);
                let idx = (!s.is_empty()).then_some(s).and_then(|v| v.parse::<usize>().ok());
                let Some(idx) = idx.filter(|&i| i >= 1 && i <= self.doc.random_texts.len()) else {
                    return Err(kit::fail("无效的序号"));
                };
                kit::update_ok(&self.service.store, |d| {
                    if idx <= d.random_texts.len() {
                        d.random_texts.remove(idx - 1);
                    }
                })?;
                self.inv.edit_text("✅ 文本已删除").await?;
                Ok(())
            }
            "on" | "off" => {
                let enabled = action == "on";
                self.change(|u| {
                    if enabled {
                        u.mode = if u.show_time() { "both" } else { "text" }.into();
                        if let Some(comp) = &mut u.display_components
                            && !comp.iter().any(|c| c == "text")
                        {
                            comp.push("text".into());
                        }
                    } else {
                        u.mode = "time".into();
                        if let Some(comp) = &mut u.display_components {
                            comp.retain(|c| c != "text");
                        }
                    }
                    u.display_order = with_component(&u.display_order, "text", enabled);
                })
                .await?;
                self.inv.edit_text(&format!("✅ 随机文本已{}", kit::on_off_text(enabled))).await?;
                Ok(())
            }
            _ => Err(kit::usage(self.inv.prefix(), "acn text add|list|del|clear|on|off")),
        }
    }

    async fn weather(&self) -> CmdResult {
        let action = self.inv.arg(1).to_ascii_lowercase();
        if action.is_empty() || action == "help" {
            let now_ms = Utc::now().timestamp_millis();
            let (preview, fetched) = current_weather(&self.inv.cancel, &self.user, true, now_ms).await;
            if fetched {
                let pv = preview.clone();
                self.mutate(move |u| {
                    u.weather_compact = pv;
                    u.weather_cache_ts = now_ms;
                })?;
            }
            self.inv.edit(&format!(
                "天气：{}\n地点：{}\n预览：{}",
                kit::on_off_text(self.user.weather_enabled),
                escape(kit::or_default(&self.user.weather_location, "未设置")),
                escape(kit::or_default(&preview, "暂无缓存"))
            )).await?;
            return Ok(());
        }
        if action == "on" && self.user.weather_location.is_empty() {
            return Err(kit::fail("请先设置地点"));
        }
        if action == "on" || action == "off" {
            let on = action == "on";
            self.change(|u| {
                u.weather_enabled = on;
                u.display_order = with_component(&u.display_order, "weather", on);
            })
            .await?;
            self.inv.edit_text("✅ 天气配置已更新").await?;
            return Ok(());
        }
        let loc = if action == "set" { self.inv.raw_after(2) } else { self.inv.raw_after(1) }.trim().to_owned();
        if loc.is_empty() {
            return Err(kit::fail("请提供地点"));
        }
        self.change(|u| {
            u.weather_location = loc;
            u.weather_enabled = true;
            u.weather_compact.clear();
            u.weather_cache_ts = 0;
            u.display_order = with_component(&u.display_order, "weather", true);
        })
        .await?;
        self.inv.edit_text("✅ 天气配置已更新").await?;
        Ok(())
    }
}

async fn acn_save(inv: &Invocation, service: &AcnService, user_id: &str) -> CmdResult {
    let self_u = current_self(&inv.client)
        .await
        .map_err(|e| {
            if inv.cancel.is_cancelled() {
                e
            } else {
                kit::fail_with("读取当前昵称失败", mibot_command::UserError::new(e.to_string()))
            }
        })?;
    let mut first = false;
    let mut saved = AcnUser::default();
    kit::update_ok(&service.store, |doc| {
        let cur = doc.users.entry(user_id.to_owned()).or_insert_with(|| AcnUser::new(user_id));
        first = cur.original_first_name.is_empty();
        cur.original_first_name = clean_nickname(&self_u.first_name.unwrap_or_default());
        cur.original_last_name = clean_nickname(&self_u.last_name.unwrap_or_default());
        if cur.timezone.is_empty() {
            cur.timezone = "Asia/Shanghai".into();
        }
        saved = cur.clone();
    })?;
    let names = format!(
        "姓名：{}\n姓氏：{}",
        code(&saved.original_first_name),
        code(kit::or_default(&saved.original_last_name, "无"))
    );
    if first {
        inv.edit(&format!(
            "🎉 <b>昵称已保存</b>\n\n{names}\n\n接下来用 {} 开启自动更新。",
            code(&format!("{}acn on", inv.prefix()))
        )).await?;
    } else {
        inv.edit(&format!("✅ <b>原始昵称已更新</b>，其他设置保留\n\n{names}")).await?;
    }
    Ok(())
}

async fn acn_handle(inv: &Invocation, service: &AcnService) -> CmdResult {
    let user_id = inv.client.self_id().to_string();
    let sub = inv.arg(0).to_ascii_lowercase();
    if sub.is_empty() {
        return inv.edit(&acn_help(inv.prefix())).await.map_err(Into::into);
    }
    let doc = service.store.read()?;
    match sub.as_str() {
        "status" => {
            let enabled = usize::from(doc.users.get(&user_id).is_some_and(|u| u.enabled));
            let running = if enabled > 0 { "运行中" } else { "已停止" };
            return inv
                .edit(&format!("📊 自动更新：{}\n开启的用户：{}", code(running), code(&enabled.to_string())))
                .await
                .map_err(Into::into);
        }
        "save" => return acn_save(inv, service, &user_id).await,
        _ => {}
    }
    let Some(user) = doc.users.get(&user_id).cloned().filter(|u| !u.original_first_name.is_empty()) else {
        return Err(kit::fail(format!("请先 {}acn save", inv.prefix())));
    };
    let call = AcnCall {
        inv,
        service,
        doc,
        user_id,
        user,
        sub: sub.clone(),
    };
    match sub.as_str() {
        "on" | "enable" | "off" | "disable" => call.toggle().await,
        "mode" => call.mode().await,
        "tz" | "timezone" => call.timezone().await,
        "text" => call.text().await,
        "emoji" | "time" => call.flag().await,
        "style" => call.style().await,
        "order" => call.order().await,
        "show" => call.show().await,
        "weather" => call.weather().await,
        "config" => call.config().await,
        "update" | "now" => call.update_now().await,
        "reset" => call.reset().await,
        _ => Err(kit::fail(format!("未知子命令：{sub}，{}acn help 看用法", inv.prefix()))),
    }
}

fn acn_help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "🏷 <b>自动昵称</b>\n\n<b>快速开始</b>\n1. <code>{p}acn save</code> 保存当前昵称（首次必须）\n2. <code>{p}acn on</code> 开启，之后每分钟自动更新\n\n<b>基础</b>\n• <code>{p}acn on</code> / <code>off</code> 开关\n• <code>{p}acn mode</code> 循环切换 time → text → both\n• <code>{p}acn update</code> 立即更新一次\n• <code>{p}acn reset</code> 恢复原始昵称并停用\n• <code>{p}acn status</code> / <code>config</code> 查看状态\n\n<b>时区</b>\n• <code>{p}acn tz Asia/Shanghai</code> 设置时区\n• <code>{p}acn tz on</code> / <code>off</code> 是否显示时区\n• <code>{p}acn tz format GMT|UTC|simp|offset|custom:文字</code>\n\n<b>外观</b>\n• <code>{p}acn emoji on</code> / <code>off</code> 时钟表情\n• <code>{p}acn time on</code> / <code>off</code> 时间显示\n• <code>{p}acn time 12</code> / <code>24</code> 12 或 24 小时制\n• <code>{p}acn style normal|italic|double|sans|mono|outline</code>\n• <code>{p}acn order name,text,time,weather,emoji,timezone</code>（同时开关其中的时间、表情、时区）\n• <code>{p}acn show time|text|weather on</code> / <code>off</code> 只显示选中的组件，<code>{p}acn show reset</code> 恢复\n\n<b>文案</b>\n• <code>{p}acn text add 摸鱼中</code>（支持多行）\n• <code>{p}acn text list</code> / <code>del 序号</code> / <code>clear</code>\n• <code>{p}acn text on</code> / <code>off</code>\n\n<b>天气</b>\n• <code>{p}acn weather set 北京</code> 设置地点并开启\n• <code>{p}acn weather on</code> / <code>off</code>\n天气缓存 30 分钟。"
    )
}

pub fn register(app: &Arc<App>) {
    let svc = Arc::new(AcnService {
        store: Arc::new(kit::new_store(app, "acn.json", AcnDoc::default)),
        logs: Some(Arc::clone(&app.logs)),
    });
    if !app.read_only {
        svc.normalize_stored(app);
    }
    let s_cmd = Arc::clone(&svc);
    app.registry.register(Command {
        name: "acn".into(),
        aliases: vec!["autochangename".into()],
        group: group::ACCOUNT.into(),
        description: "按时间或天气自动改昵称".into(),
        usage: "[子命令]".into(),
        help_fn: Some(Arc::new(acn_help)),
        handle: Arc::new(move |inv| {
            let s = Arc::clone(&s_cmd);
            Box::pin(async move { acn_handle(&inv, &s).await })
        }),
        ..Default::default()
    });

    let s_job = Arc::clone(&svc);
    app.on_ready(Arc::new(move |app, cancel| {
        let s = Arc::clone(&s_job);
        Box::pin(async move {
            loop {
                let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                let rem = 60_000 - (now_ms % 60_000);
                if !kit::sleep(&cancel, Duration::from_millis(rem)).await {
                    return;
                }
                s.tick(&cancel, &app.client).await;
            }
        })
    }));
    let _ = AcnState::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::InvocationError;
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder};
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-acn-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn winter() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 15, 6, 32, 0).unwrap()
    }

    fn summer() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 15, 6, 32, 0).unwrap()
    }

    #[test]
    fn test_styles_zones_round_compose_order_mibox_normalize_clean() {
        assert_eq!(apply_text_style("ab12", "mono"), "𝚊𝚋𝟷𝟸");
        assert_eq!(apply_text_style("CZ", "double"), "ℂℤ");
        assert!(apply_text_style("中文 ok", "sans").starts_with("中文 "));
        assert_eq!(apply_text_style("abc", "normal"), "abc");

        let w = winter();
        let s = summer();
        for (fmt, want) in [
            ("GMT", "GMT+8"),
            ("UTC", "UTC+8"),
            ("OFFSET", "+08:00"),
            ("SIMP", "CST"),
            ("custom:北京时间", "北京时间"),
        ] {
            assert_eq!(zone_label("Asia/Shanghai", fmt, w), want);
        }
        assert_eq!(zone_label("UTC", "GMT", w), "GMT");
        assert_eq!(zone_label("Asia/Kolkata", "GMT", w), "GMT+05:30");
        assert_eq!(zone_label("Not/AZone", "GMT", w), "");

        for (zone, at, want) in [
            ("Asia/Singapore", w, "SGT"),
            ("Asia/Bangkok", w, "ICT"),
            ("Asia/Dubai", w, "GST"),
            ("America/New_York", s, "EDT"),
            ("America/New_York", w, "EST"),
            ("Pacific/Chatham", w, "CHADT"),
            ("Pacific/Chatham", s, "CHAST"),
            ("America/Santiago", s, "CLT"),
            ("Etc/GMT-5", w, "PKT"),
            ("Etc/GMT-3", w, "MSK"),
            ("Asia/Kathmandu", w, "NPT"),
            ("Pacific/Marquesas", w, "MART"),
        ] {
            assert_eq!(zone_label(zone, "simp", at), want, "zone={zone}");
        }

        assert_eq!(zone_time("Asia/Shanghai", "24", w), "14:32");
        assert_eq!(zone_time("Asia/Shanghai", "12", w), "02:32 PM");
        assert_eq!(clock_emoji("Asia/Shanghai", w), "🕑");

        assert_eq!(format_weather(0, -3.2), "☀️ -3°C");
        assert_eq!(format_weather(999, -0.4), "🌤️ 0°C");

        let mut u = AcnUser::new("100");
        u.original_first_name = "Cat".into();
        u.display_order.clear();
        u.mode = "both".into();
        u.show_clock_emoji = true;
        u.show_timezone = true;
        assert_eq!(compose_name(&u, &["摸鱼中".into()], "☀️ 21°C", w), "Cat 14:32 摸鱼中 ☀️ 21°C 🕑 GMT+8");

        let raw = r#"{"users":{"123456789":{"user_id":123456789,"timezone":"Asia/Tokyo","original_first_name":"Cat","is_enabled":true,"mode":"both","last_update":null,"text_index":2,"hour_format":"12","displayComponents":["time","text"],"future_flag":{"nested":[1,2]}}},"random_texts":["a"]}"#;
        let doc: AcnDoc = serde_json::from_str(raw).unwrap();
        let u1 = &doc.users["123456789"];
        assert_eq!(u1.user_id, "123456789");
        assert_eq!(u1.hour_format, "12");
        let encoded = serde_json::to_string(&doc).unwrap();
        assert!(encoded.contains(r#""user_id":"123456789""#));
        assert!(encoded.contains(r#""future_flag":{"nested":[1,2]}"#));

        assert_eq!(clean_nickname("我要一直陪着我 🕘 09:30"), "我要一直陪着我");
        assert_eq!(clean_nickname("  Alice   Smith  "), "Alice Smith");
        assert_eq!(clean_nickname(&format!("Cat {}", zone_time("Asia/Shanghai", "12", w))), "Cat");
    }

    #[tokio::test]
    async fn test_foreign_config_and_restore_on_disable() {
        let dir = temp_dir("rec");
        let store = Arc::new(Store::new(dir.join("acn.json"), AcnDoc::default));
        kit::update_ok(&store, |d| {
            let mut u = AcnUser::new("2");
            u.enabled = true;
            u.original_first_name = "ForeignAccount".into();
            d.users.insert("2".into(), u);
        })
        .unwrap();

        let names = Arc::new(StdMutex::new(Vec::<String>::new()));
        let names_ref = Arc::clone(&names);
        let store_ref = Arc::clone(&store);
        struct ProfileFake {
            names: Arc<StdMutex<Vec<String>>>,
            disable_on_first: bool,
            store: Arc<Store<AcnDoc>>,
        }
        impl RpcTransport for ProfileFake {
            fn invoke_raw<'a>(
                &'a self,
                _dc_id: Option<i32>,
                cid: u32,
                body: Vec<u8>,
            ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async move {
                    if cid == tl::functions::account::UpdateProfile::CONSTRUCTOR_ID {
                        let req = tl::functions::account::UpdateProfile::from_bytes(&body[4..]).unwrap();
                        let first = req.first_name.unwrap_or_default();
                        let mut g = self.names.lock().unwrap();
                        g.push(first);
                        if self.disable_on_first && g.len() == 1 {
                            kit::update_ok(&self.store, |d| {
                                if let Some(u) = d.users.get_mut("1") {
                                    u.enabled = false;
                                }
                            })
                            .unwrap();
                        }
                        let resp: tl::enums::User = tl::types::User {
                            is_self: true,
                            ..kit::blank_tl_user(1)
                        }
                        .into();
                        return Ok(resp.to_bytes());
                    }
                    Err(InvocationError::Dropped("unexpected"))
                })
            }
        }
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::new(ProfileFake {
                names: Arc::clone(&names_ref),
                disable_on_first: false,
                store: Arc::clone(&store_ref),
            }),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let svc = AcnService { store: Arc::clone(&store), logs: None };
        let cancel = CancellationToken::new();
        svc.tick(&cancel, &client).await;
        assert!(names.lock().unwrap().is_empty());

        kit::update_ok(&store, |d| {
            let mut u = AcnUser::new("1");
            u.enabled = true;
            u.original_first_name = "Self".into();
            d.users.insert("1".into(), u);
        })
        .unwrap();
        svc.tick(&cancel, &client).await;
        assert_eq!(names.lock().unwrap().len(), 1);
        assert!(names.lock().unwrap()[0].starts_with("Self "));

        // Disable during update restores original
        let names2 = Arc::new(StdMutex::new(Vec::new()));
        let store2 = Arc::new(Store::new(dir.join("acn2.json"), AcnDoc::default));
        kit::update_ok(&store2, |d| {
            let mut u = AcnUser::new("1");
            u.enabled = true;
            u.original_first_name = "Original".into();
            d.users.insert("1".into(), u);
        })
        .unwrap();
        let client2 = Arc::new(Client::from_transport(
            Arc::new(ProfileFake {
                names: Arc::clone(&names2),
                disable_on_first: true,
                store: Arc::clone(&store2),
            }),
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let svc2 = AcnService { store: store2, logs: None };
        svc2.tick(&cancel, &client2).await;
        let g2 = names2.lock().unwrap();
        assert_eq!(g2.len(), 2);
        assert_eq!(g2[1], "Original");
        assert_ne!(g2[0], g2[1]);
        let _ = std::fs::remove_dir_all(dir);
    }
}

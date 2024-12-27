#![allow(clippy::type_complexity)]
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use anyhow::{anyhow, Result};
use bgpkit_parser::encoder::MrtUpdatesEncoder;
use bgpkit_parser::BgpElem;
use clap::{Args, Parser, Subcommand};
use ipnet::IpNet;
use json_to_table::json_to_table;
use monocle::*;
use radar_rs::RadarClient;
use rayon::prelude::*;
use serde_json::json;
use tabled::settings::{Merge, Style};
use tabled::{Table, Tabled};
use tracing::{info, Level};

trait Validate {
    fn validate(&self) -> Result<()>;
}

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
#[clap(propagate_version = true)]
struct Cli {
    /// configuration file path, by default $HOME/.monocle.toml is used
    #[clap(short, long)]
    config: Option<String>,

    /// Print debug information
    #[clap(long, global = true)]
    debug: bool,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(Args, Debug)]
struct ParseFilters {
    /// Filter by origin AS Number
    #[clap(short = 'o', long)]
    origin_asn: Option<u32>,

    /// Filter by network prefix
    #[clap(short = 'p', long)]
    prefix: Option<String>,

    /// Include super-prefix when filtering
    #[clap(short = 's', long)]
    include_super: bool,

    /// Include sub-prefix when filtering
    #[clap(short = 'S', long)]
    include_sub: bool,

    /// Filter by peer IP address
    #[clap(short = 'j', long)]
    peer_ip: Vec<IpAddr>,

    /// Filter by peer ASN
    #[clap(short = 'J', long)]
    peer_asn: Option<u32>,

    /// Filter by elem type: announce (a) or withdraw (w)
    #[clap(short = 'm', long)]
    elem_type: Option<String>,

    /// Filter by start unix timestamp inclusive
    #[clap(short = 't', long)]
    start_ts: Option<String>,

    /// Filter by end unix timestamp inclusive
    #[clap(short = 'T', long)]
    end_ts: Option<String>,

    /// Filter by AS path regex string
    #[clap(short = 'a', long)]
    as_path: Option<String>,
}

#[derive(Args, Debug)]
struct SearchFilters {
    /// Filter by start unix timestamp inclusive
    #[clap(short = 't', long)]
    start_ts: Option<String>,

    /// Filter by end unix timestamp inclusive
    #[clap(short = 'T', long)]
    end_ts: Option<String>,

    #[clap(short = 'd', long)]
    duration: Option<String>,

    /// Filter by collector, e.g. rrc00 or route-views2
    #[clap(short = 'c', long)]
    collector: Option<String>,

    /// Filter by route collection project, i.e. riperis or routeviews
    #[clap(short = 'P', long)]
    project: Option<String>,

    /// Filter by origin AS Number
    #[clap(short = 'o', long)]
    origin_asn: Option<u32>,

    /// Filter by network prefix
    #[clap(short = 'p', long)]
    prefix: Option<String>,

    /// Include super-prefix when filtering
    #[clap(short = 's', long)]
    include_super: bool,

    /// Include sub-prefix when filtering
    #[clap(short = 'S', long)]
    include_sub: bool,

    /// Filter by peer IP address
    #[clap(short = 'j', long)]
    peer_ip: Vec<IpAddr>,

    /// Filter by peer ASN
    #[clap(short = 'J', long)]
    peer_asn: Option<u32>,

    /// Filter by elem type: announce (a) or withdraw (w)
    #[clap(short = 'm', long)]
    elem_type: Option<String>,

    /// Filter by AS path regex string
    #[clap(short = 'a', long)]
    as_path: Option<String>,
}

impl Validate for ParseFilters {
    fn validate(&self) -> Result<()> {
        if let Some(ts) = &self.start_ts {
            if string_to_time(ts.as_str()).is_err() {
                return Err(anyhow!("start-ts is not a valid time string: {}", ts));
            }
        }
        if let Some(ts) = &self.end_ts {
            if string_to_time(ts.as_str()).is_err() {
                return Err(anyhow!("end-ts is not a valid time string: {}", ts));
            }
        }
        Ok(())
    }
}

impl SearchFilters {
    fn parse_start_end_strings(&self) -> Result<(i64, i64)> {
        let mut start_ts = None;
        let mut end_ts = None;
        if let Some(ts) = &self.start_ts {
            match string_to_time(ts.as_str()) {
                Ok(t) => start_ts = Some(t),
                Err(_) => return Err(anyhow!("start-ts is not a valid time string: {}", ts)),
            }
        }
        if let Some(ts) = &self.end_ts {
            match string_to_time(ts.as_str()) {
                Ok(t) => end_ts = Some(t),
                Err(_) => return Err(anyhow!("end-ts is not a valid time string: {}", ts)),
            }
        }

        match (&self.start_ts, &self.end_ts, &self.duration) {
            (Some(_), Some(_), Some(_)) => {
                return Err(anyhow!(
                    "cannot specify start_ts, end_ts, and duration all at the same time"
                ))
            }
            (Some(_), None, None) | (None, Some(_), None) => {
                // only one start_ts or end_ts specified
                return Err(anyhow!(
                    "must specify two from: start_ts, end_ts and duration"
                ));
            }
            (None, None, _) => {
                return Err(anyhow!(
                    "must specify two from: start_ts, end_ts and duration"
                ));
            }
            _ => {}
        }
        if let Some(duration) = &self.duration {
            // this case is duration + start_ts OR end_ts
            let duration = match humantime::parse_duration(duration) {
                Ok(d) => d,
                Err(_) => {
                    return Err(anyhow!(
                        "duration is not a valid time duration string: {}",
                        duration
                    ))
                }
            };

            if let Some(ts) = start_ts {
                return Ok((ts.timestamp(), (ts + duration).timestamp()));
            }
            if let Some(ts) = end_ts {
                return Ok(((ts - duration).timestamp(), ts.timestamp()));
            }
        } else {
            // this case is start_ts AND end_ts
            return Ok((start_ts.unwrap().timestamp(), end_ts.unwrap().timestamp()));
        }

        Err(anyhow!("unexpected time-string parsing result"))
    }
}
impl Validate for SearchFilters {
    fn validate(&self) -> Result<()> {
        let _ = self.parse_start_end_strings()?;
        Ok(())
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Parse individual MRT files given a file path, local or remote.
    Parse {
        /// File path to a MRT file, local or remote.
        #[clap(name = "FILE")]
        file_path: PathBuf,

        /// Output as JSON objects
        #[clap(long)]
        json: bool,

        /// Pretty-print JSON output
        #[clap(long)]
        pretty: bool,

        /// MRT output file path
        #[clap(long, short = 'M')]
        mrt_path: Option<PathBuf>,

        /// Filter by AS path regex string
        #[clap(flatten)]
        filters: ParseFilters,
    },

    /// Search BGP messages from all available public MRT files.
    Search {
        /// Dry-run, do not download or parse.
        #[clap(long)]
        dry_run: bool,

        /// Output as JSON objects
        #[clap(long)]
        json: bool,

        /// Pretty-print JSON output
        #[clap(long)]
        pretty: bool,

        /// SQLite output file path
        #[clap(long)]
        sqlite_path: Option<PathBuf>,

        /// MRT output file path
        #[clap(long, short = 'M')]
        mrt_path: Option<PathBuf>,

        /// SQLite reset database content if exists
        #[clap(long)]
        sqlite_reset: bool,

        /// Filter by AS path regex string
        #[clap(flatten)]
        filters: SearchFilters,
    },
    /// ASN and organization lookup utility.
    Whois {
        /// Search query, an ASN (e.g. "400644") or a name (e.g. "bgpkit")
        query: Vec<String>,

        /// Search AS and Org name only
        #[clap(short, long)]
        name_only: bool,

        /// Search by ASN only
        #[clap(short, long)]
        asn_only: bool,

        /// Search by country only
        #[clap(short = 'C', long)]
        country_only: bool,

        /// Refresh local as2org database
        #[clap(short, long)]
        update: bool,

        /// Output to pretty table, default markdown table
        #[clap(short, long)]
        pretty: bool,

        /// Display full table (with ord_id, org_size)
        #[clap(short = 'F', long)]
        full_table: bool,

        /// Export to pipe-separated values
        #[clap(short = 'P', long)]
        psv: bool,

        /// Show full country names instead of 2-letter code
        #[clap(short, long)]
        full_country: bool,
    },

    /// Country name and code lookup utilities
    Country {
        /// Search query, e.g. "US" or "United States"
        queries: Vec<String>,
    },

    /// Time conversion utilities
    Time {
        /// Time stamp or time string to convert
        #[clap()]
        time: Vec<String>,

        /// Simple output, only print the converted time
        #[clap(short, long)]
        simple: bool,
    },

    /// RPKI utilities
    Rpki {
        #[clap(subcommand)]
        commands: RpkiCommands,
    },

    /// IP information lookup
    Ip {
        /// IP address to look up (optional)
        #[clap()]
        ip: Option<IpAddr>,

        /// Print IP address only (e.g. for getting the public IP address quickly)
        #[clap(long)]
        simple: bool,

        /// Output as JSON objects
        #[clap(long)]
        json: bool,
    },

    /// Cloudflare Radar API lookup (set CF_API_TOKEN to enable)
    Radar {
        #[clap(subcommand)]
        commands: RadarCommands,
    },
}

#[derive(Subcommand)]
enum RpkiCommands {
    /// parse a RPKI ROA file
    ReadRoa {
        /// File path to a ROA file (.roa), local or remote.
        #[clap(name = "FILE")]
        file_path: PathBuf,
    },

    /// parse a RPKI ASPA file
    ReadAspa {
        /// File path to a ASPA file (.asa), local or remote.
        #[clap(name = "FILE")]
        file_path: PathBuf,

        #[clap(long)]
        no_merge_dups: bool,
    },

    /// validate a prefix-asn pair with a RPKI validator
    Check {
        #[clap(short, long)]
        asn: u32,

        #[clap(short, long)]
        prefix: String,
    },

    /// list ROAs by ASN or prefix
    List {
        /// prefix or ASN
        #[clap()]
        resource: String,
    },

    /// summarize RPKI status for a list of given ASNs
    Summary {
        #[clap()]
        asns: Vec<u32>,
    },
}

#[derive(Subcommand)]
enum RadarCommands {
    /// get routing stats
    Stats {
        /// a two-letter country code or asn number (e.g. US or 13335)
        #[clap(name = "QUERY")]
        query: Option<String>,
    },

    /// look up prefix to origin mapping on the most recent global routing table snapshot
    Pfx2as {
        /// a IP prefix or an AS number (e.g. 1.1.1.0/24 or 13335)
        #[clap(name = "QUERY")]
        query: String,

        /// filter by RPKI validation status, valid, invalid, or unknown
        #[clap(short, long)]
        rpki_status: Option<String>,
    },
}

fn elem_to_string(elem: &BgpElem, json: bool, pretty: bool, collector: &str) -> String {
    if json {
        let mut val = json!(elem);
        val.as_object_mut()
            .unwrap()
            .insert("collector".to_string(), collector.into());
        if pretty {
            serde_json::to_string_pretty(&val).unwrap()
        } else {
            val.to_string()
        }
    } else {
        format!("{}|{}", elem, collector)
    }
}

fn main() {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    let config = MonocleConfig::new(&cli.config);

    if cli.debug {
        tracing_subscriber::fmt()
            // filter spans/events with level TRACE or higher.
            .with_max_level(Level::INFO)
            .init();
    }

    // You can check for the existence of subcommands, and if found use their
    // matches just as you would the top level cmd
    match cli.command {
        Commands::Parse {
            file_path,
            json,
            pretty,
            mrt_path,
            filters,
        } => {
            if let Err(e) = filters.validate() {
                eprintln!("ERROR: {e}");
                return;
            }

            let file_path = file_path.to_str().unwrap();
            let parser = parser_with_filters(
                file_path,
                &filters.origin_asn,
                &filters.prefix,
                &filters.include_super,
                &filters.include_sub,
                &filters.peer_ip,
                &filters.peer_asn,
                &filters.elem_type,
                &filters.start_ts.clone(),
                &filters.end_ts.clone(),
                &filters.as_path,
            )
            .unwrap();

            let mut stdout = std::io::stdout();

            match mrt_path {
                None => {
                    for elem in parser {
                        // output to stdout
                        let output_str = elem_to_string(&elem, json, pretty, "");
                        if let Err(e) = writeln!(stdout, "{}", &output_str) {
                            if e.kind() != std::io::ErrorKind::BrokenPipe {
                                eprintln!("ERROR: {e}");
                            }
                            std::process::exit(1);
                        }
                    }
                }
                Some(p) => {
                    let path = p.to_str().unwrap().to_string();
                    println!("processing. filtered messages output to {}...", &path);
                    let mut encoder = MrtUpdatesEncoder::new();
                    let mut writer = match oneio::get_writer(&path) {
                        Ok(w) => w,
                        Err(e) => {
                            eprintln!("ERROR: {e}");
                            std::process::exit(1);
                        }
                    };
                    let mut total_count = 0;
                    for elem in parser {
                        total_count += 1;
                        encoder.process_elem(&elem);
                    }
                    writer.write_all(&encoder.export_bytes()).unwrap();
                    drop(writer);
                    println!("done. total of {} message wrote", total_count);
                }
            }
        }
        Commands::Search {
            dry_run,
            json,
            pretty,
            mrt_path,
            sqlite_path,
            sqlite_reset,
            filters,
        } => {
            if let Err(e) = filters.validate() {
                eprintln!("ERROR: {e}");
                return;
            }

            let mut sqlite_path_str = "".to_string();
            let sqlite_db = sqlite_path.map(|p| {
                sqlite_path_str = p.to_str().unwrap().to_string();
                MsgStore::new(&Some(sqlite_path_str.clone()), sqlite_reset)
            });
            let mrt_path = mrt_path.map(|p| p.to_str().unwrap().to_string());
            let show_progress = sqlite_db.is_some() || mrt_path.is_some();

            // it's fine to unwrap as the filters.validate() function has already checked for issues
            let (ts_start, ts_end) = filters.parse_start_end_strings().unwrap();

            let mut broker = bgpkit_broker::BgpkitBroker::new()
                .ts_start(ts_start)
                .ts_end(ts_end)
                .data_type("update")
                .page_size(1000);

            if let Some(project) = filters.project {
                broker = broker.project(project.as_str());
            }
            if let Some(collector) = filters.collector {
                broker = broker.collector_id(collector.as_str());
            }

            let items = broker
                .query()
                .expect("broker query error: please check filters are valid");

            let total_items = items.len();

            let total_size: i64 = items
                .iter()
                .map(|x| {
                    info!(
                        "{},{},{}",
                        x.collector_id.as_str(),
                        x.url.as_str(),
                        x.rough_size
                    );
                    x.rough_size
                })
                .sum::<i64>();
            info!(
                "total of {} files, {} bytes to parse",
                items.len(),
                total_size
            );

            if dry_run {
                println!(
                    "total of {} files, {} bytes to parse",
                    items.len(),
                    total_size
                );
                return;
            }

            let (sender, receiver): (Sender<(BgpElem, String)>, Receiver<(BgpElem, String)>) =
                channel();
            // progress bar
            let (pb_sender, pb_receiver): (Sender<u32>, Receiver<u32>) = channel();

            // dedicated thread for handling output of results
            let writer_thread = thread::spawn(move || {
                let display_stdout = sqlite_db.is_none() && mrt_path.is_none();
                let mut mrt_writer = mrt_path.map(|p| {
                    (
                        MrtUpdatesEncoder::new(),
                        oneio::get_writer(p.as_str()).unwrap(),
                    )
                });

                let mut msg_cache = vec![];
                let mut msg_count = 0;

                for (elem, collector) in receiver {
                    msg_count += 1;

                    if display_stdout {
                        let output_str = elem_to_string(&elem, json, pretty, collector.as_str());
                        println!("{output_str}");
                        continue;
                    }

                    msg_cache.push((elem, collector));
                    if msg_cache.len() >= 100000 {
                        if let Some(db) = &sqlite_db {
                            db.insert_elems(&msg_cache);
                        }
                        if let Some((encoder, _writer)) = &mut mrt_writer {
                            for (elem, _) in &msg_cache {
                                encoder.process_elem(elem);
                            }
                        }
                        msg_cache.clear();
                    }
                }

                if !msg_cache.is_empty() {
                    if let Some(db) = &sqlite_db {
                        db.insert_elems(&msg_cache);
                    }
                    if let Some((encoder, _writer)) = &mut mrt_writer {
                        for (elem, _) in &msg_cache {
                            encoder.process_elem(elem);
                        }
                    }
                }
                if let Some((encoder, writer)) = &mut mrt_writer {
                    let bytes = encoder.export_bytes();
                    writer.write_all(&bytes).unwrap();
                }
                drop(mrt_writer);

                if !display_stdout {
                    println!("processed {total_items} files, found {msg_count} messages, written into file {sqlite_path_str}");
                }
            });

            // dedicated thread for progress bar
            let progress_thread = thread::spawn(move || {
                if !show_progress {
                    return;
                }

                let sty = indicatif::ProgressStyle::with_template(
                    "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {eta} left; {msg}",
                )
                .unwrap()
                .progress_chars("##-");
                let pb = indicatif::ProgressBar::new(total_items as u64);
                pb.set_style(sty);
                let mut total_count: u64 = 0;
                for count in pb_receiver.iter() {
                    total_count += count as u64;
                    pb.set_message(format!("found {total_count} messages"));
                    pb.inc(1);
                }
            });

            items
                .into_par_iter()
                .for_each_with((sender, pb_sender), |(s, pb_sender), item| {
                    let url = item.url;
                    let collector = item.collector_id;
                    info!("start parsing {}", url.as_str());
                    let parser = parser_with_filters(
                        url.as_str(),
                        &filters.origin_asn,
                        &filters.prefix,
                        &filters.include_super,
                        &filters.include_sub,
                        &filters.peer_ip,
                        &filters.peer_asn,
                        &filters.elem_type,
                        // use the parsed new start and end ts
                        &Some(ts_start.to_string()),
                        &Some(ts_end.to_string()),
                        &filters.as_path,
                    )
                    .unwrap();

                    let mut elems_count = 0;
                    for elem in parser {
                        s.send((elem, collector.clone())).unwrap();
                        elems_count += 1;
                    }

                    if show_progress {
                        pb_sender.send(elems_count).unwrap();
                    }
                    info!("finished parsing {}", url.as_str());
                });

            // wait for the output thread to stop
            writer_thread.join().unwrap();
            progress_thread.join().unwrap();
        }
        Commands::Whois {
            query,
            name_only,
            asn_only,
            update,
            pretty,
            full_table,
            full_country,
            country_only,
            psv,
        } => {
            let data_dir = config.data_dir.as_str();
            let as2org = As2org::new(&Some(format!("{data_dir}/monocle-data.sqlite3"))).unwrap();

            if update {
                // if update flag is set, clear existing as2org data and re-download later
                as2org.clear_db();
            }

            if as2org.is_db_empty() {
                println!("bootstrapping as2org data now... (it will take about one minute)");
                as2org.parse_insert_as2org(None).unwrap();
                println!("bootstrapping as2org data finished");
            }

            let mut search_type: SearchType = match (name_only, asn_only) {
                (true, false) => SearchType::NameOnly,
                (false, true) => SearchType::AsnOnly,
                (false, false) => SearchType::Guess,
                (true, true) => {
                    eprintln!("ERROR: name-only and asn-only cannot be both true");
                    return;
                }
            };

            if country_only {
                search_type = SearchType::CountryOnly;
            }

            let mut res = query
                .into_iter()
                .flat_map(|q| {
                    as2org
                        .search(q.as_str(), &search_type, full_country)
                        .unwrap()
                })
                .collect::<Vec<SearchResult>>();

            // order search results by AS number
            res.sort_by_key(|v| v.asn);

            match full_table {
                false => {
                    let res_concise = res.into_iter().map(|x: SearchResult| SearchResultConcise {
                        asn: x.asn,
                        as_name: x.as_name,
                        org_name: x.org_name,
                        org_country: x.org_country,
                    });
                    if psv {
                        println!("asn|asn_name|org_name|org_country");
                        for res in res_concise {
                            println!(
                                "{}|{}|{}|{}",
                                res.asn, res.as_name, res.org_name, res.org_country
                            );
                        }
                        return;
                    }

                    match pretty {
                        true => {
                            println!("{}", Table::new(res_concise).with(Style::rounded()));
                        }
                        false => {
                            println!("{}", Table::new(res_concise).with(Style::markdown()));
                        }
                    };
                }
                true => {
                    if psv {
                        println!("asn|asn_name|org_name|org_id|org_country|org_size");
                        for entry in res {
                            println!(
                                "{}|{}|{}|{}|{}|{}",
                                entry.asn,
                                entry.as_name,
                                entry.org_name,
                                entry.org_id,
                                entry.org_country,
                                entry.org_size
                            );
                        }
                        return;
                    }
                    match pretty {
                        true => {
                            println!("{}", Table::new(res).with(Style::rounded()));
                        }
                        false => {
                            println!("{}", Table::new(res).with(Style::markdown()));
                        }
                    };
                }
            }
        }
        Commands::Time { time, simple } => {
            let timestring_res = match simple {
                true => parse_time_string_to_rfc3339(&time),
                false => time_to_table(&time),
            };
            match timestring_res {
                Ok(t) => {
                    println!("{t}")
                }
                Err(e) => {
                    eprintln!("ERROR: {e}")
                }
            };
        }
        Commands::Country { queries } => {
            let lookup = CountryLookup::new();
            let res: Vec<CountryEntry> = queries
                .into_iter()
                .flat_map(|query| lookup.lookup(query.as_str()))
                .collect();
            println!("{}", Table::new(res).with(Style::rounded()));
        }
        Commands::Rpki { commands } => match commands {
            RpkiCommands::ReadRoa { file_path } => {
                let res = match read_roa(file_path.to_str().unwrap()) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("ERROR: unable to read ROA file: {}", e);
                        return;
                    }
                };
                println!("{}", Table::new(res).with(Style::markdown()));
            }
            RpkiCommands::ReadAspa {
                file_path,
                no_merge_dups,
            } => {
                let res = match read_aspa(file_path.to_str().unwrap()) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("ERROR: unable to read ASPA file: {}", e);
                        return;
                    }
                };
                match no_merge_dups {
                    true => println!("{}", Table::new(res).with(Style::markdown())),
                    false => println!(
                        "{}",
                        Table::new(res)
                            .with(Style::markdown())
                            .with(Merge::vertical())
                    ),
                };
            }
            RpkiCommands::Check { asn, prefix } => {
                let (validity, roas) = match validate(asn, prefix.as_str()) {
                    Ok((v1, v2)) => (v1, v2),
                    Err(e) => {
                        eprintln!("ERROR: unable to check RPKI validity: {}", e);
                        return;
                    }
                };
                println!("RPKI validation result:");
                println!("{}", Table::new(vec![validity]).with(Style::markdown()));
                println!();
                println!("Covering prefixes:");
                println!(
                    "{}",
                    Table::new(
                        roas.into_iter()
                            .map(RoaTableItem::from)
                            .collect::<Vec<RoaTableItem>>()
                    )
                    .with(Style::markdown())
                );
            }
            RpkiCommands::List { resource } => {
                let resources = match resource.parse::<u32>() {
                    Ok(asn) => list_by_asn(asn).unwrap(),
                    Err(_) => match resource.parse::<IpNet>() {
                        Ok(prefix) => list_by_prefix(&prefix).unwrap(),
                        Err(_) => {
                            eprintln!(
                                "ERROR: list resource not an AS number or a prefix: {}",
                                resource
                            );
                            return;
                        }
                    },
                };

                let roas: Vec<RoaTableItem> = resources
                    .into_iter()
                    .flat_map(Into::<Vec<RoaTableItem>>::into)
                    .collect();
                if roas.is_empty() {
                    println!("no matching ROAS found for {}", resource);
                } else {
                    println!("{}", Table::new(roas).with(Style::markdown()));
                }
            }
            RpkiCommands::Summary { asns } => {
                let res: Vec<SummaryTableItem> = asns
                    .into_iter()
                    .map(|v| summarize_asn(v).unwrap())
                    .collect();

                println!("{}", Table::new(res).with(Style::markdown()));
            }
        },
        Commands::Radar { commands } => {
            let client = RadarClient::new().unwrap();

            match commands {
                RadarCommands::Stats { query } => {
                    let (country, asn) = match query {
                        None => (None, None),
                        Some(q) => match q.parse::<u32>() {
                            Ok(asn) => (None, Some(asn)),
                            Err(_) => (Some(q), None),
                        },
                    };

                    let res = match client.get_bgp_routing_stats(asn, country.clone()) {
                        Ok(res) => res,
                        Err(e) => {
                            eprintln!("ERROR: unable to get routing stats: {}", e);
                            return;
                        }
                    };

                    let scope = match (country, &asn) {
                        (None, None) => "global".to_string(),
                        (Some(c), None) => c,
                        (None, Some(asn)) => format!("as{}", asn),
                        (Some(_), Some(_)) => {
                            eprintln!("ERROR: cannot specify both country and ASN");
                            return;
                        }
                    };

                    #[derive(Tabled)]
                    struct Stats {
                        pub scope: String,
                        pub origins: u32,
                        pub prefixes: u32,
                        pub rpki_valid: String,
                        pub rpki_invalid: String,
                        pub rpki_unknown: String,
                    }
                    let table_data = vec![
                        Stats {
                            scope: scope.clone(),
                            origins: res.stats.distinct_origins,
                            prefixes: res.stats.distinct_prefixes,
                            rpki_valid: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_valid,
                                (res.stats.routes_valid as f64 / res.stats.routes_total as f64)
                                    * 100.0
                            ),
                            rpki_invalid: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_invalid,
                                (res.stats.routes_invalid as f64 / res.stats.routes_total as f64)
                                    * 100.0
                            ),
                            rpki_unknown: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_unknown,
                                (res.stats.routes_unknown as f64 / res.stats.routes_total as f64)
                                    * 100.0
                            ),
                        },
                        Stats {
                            scope: format!("{} ipv4", scope),
                            origins: res.stats.distinct_origins_ipv4,
                            prefixes: res.stats.distinct_prefixes_ipv4,
                            rpki_valid: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_valid_ipv4,
                                (res.stats.routes_valid_ipv4 as f64
                                    / res.stats.routes_total_ipv4 as f64)
                                    * 100.0
                            ),
                            rpki_invalid: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_invalid_ipv4,
                                (res.stats.routes_invalid_ipv4 as f64
                                    / res.stats.routes_total_ipv4 as f64)
                                    * 100.0
                            ),
                            rpki_unknown: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_unknown_ipv4,
                                (res.stats.routes_unknown_ipv4 as f64
                                    / res.stats.routes_total_ipv4 as f64)
                                    * 100.0
                            ),
                        },
                        Stats {
                            scope: format!("{} ipv6", scope),
                            origins: res.stats.distinct_origins_ipv6,
                            prefixes: res.stats.distinct_prefixes_ipv6,
                            rpki_valid: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_valid_ipv6,
                                (res.stats.routes_valid_ipv6 as f64
                                    / res.stats.routes_total_ipv6 as f64)
                                    * 100.0
                            ),
                            rpki_invalid: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_invalid_ipv6,
                                (res.stats.routes_invalid_ipv6 as f64
                                    / res.stats.routes_total_ipv6 as f64)
                                    * 100.0
                            ),
                            rpki_unknown: format!(
                                "{} ({:.2}%)",
                                res.stats.routes_unknown_ipv6,
                                (res.stats.routes_unknown_ipv6 as f64
                                    / res.stats.routes_total_ipv6 as f64)
                                    * 100.0
                            ),
                        },
                    ];
                    println!("{}", Table::new(table_data).with(Style::modern()));
                    println!("\nData generated at {} UTC.", res.meta.data_time);
                }
                RadarCommands::Pfx2as { query, rpki_status } => {
                    let (asn, prefix) = match query.parse::<u32>() {
                        Ok(asn) => (Some(asn), None),
                        Err(_) => (None, Some(query)),
                    };

                    let rpki = if let Some(rpki_status) = rpki_status {
                        match rpki_status.to_lowercase().as_str() {
                            "valid" | "invalid" | "unknown" => Some(rpki_status),
                            _ => {
                                eprintln!("ERROR: invalid rpki status: {}", rpki_status);
                                return;
                            }
                        }
                    } else {
                        None
                    };

                    let res = match client.get_bgp_prefix_origins(asn, prefix, rpki) {
                        Ok(res) => res,
                        Err(e) => {
                            eprintln!("ERROR: unable to get prefix origins: {}", e);
                            return;
                        }
                    };

                    #[derive(Tabled)]
                    struct Pfx2origin {
                        pub prefix: String,
                        pub origin: String,
                        pub rpki: String,
                        pub visibility: String,
                    }

                    if res.prefix_origins.is_empty() {
                        println!("no prefix origins found for the given query");
                        return;
                    }

                    fn count_to_visibility(count: u32, total: u32) -> String {
                        let ratio = count as f64 / total as f64;
                        if ratio > 0.8 {
                            format!("high ({:.2}%)", ratio * 100.0)
                        } else if ratio < 0.2 {
                            format!("low ({:.2}%)", ratio * 100.0)
                        } else {
                            format!("mid ({:.2}%)", ratio * 100.0)
                        }
                    }

                    let table_data = res
                        .prefix_origins
                        .into_iter()
                        .map(|entry| Pfx2origin {
                            prefix: entry.prefix,
                            origin: format!("as{}", entry.origin),
                            rpki: entry.rpki_validation.to_lowercase(),
                            visibility: count_to_visibility(
                                entry.peer_count as u32,
                                res.meta.total_peers as u32,
                            ),
                        })
                        .collect::<Vec<Pfx2origin>>();

                    println!("{}", Table::new(table_data).with(Style::modern()));
                    println!("\nData generated at {} UTC.", res.meta.data_time);
                }
            }
        }
        Commands::Ip { ip, json, simple } => match fetch_ip_info(ip, simple) {
            Ok(ipinfo) => {
                if simple {
                    println!("{}", ipinfo.ip);
                    return;
                }

                let json_value = json!(&ipinfo);
                if json {
                    serde_json::to_writer_pretty(std::io::stdout(), &json_value).unwrap();
                } else {
                    let mut table = json_to_table(&json_value);
                    table.collapse();
                    println!("{}", table);
                }
            }
            Err(e) => {
                eprintln!("ERROR: unable to get ip information: {e}");
            }
        },
    }
}

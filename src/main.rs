use chrono::{DateTime, Utc};
use clap::{arg, value_parser, ArgAction, Command};
use clipboard::{ClipboardContext, ClipboardProvider};
use futures::future::join_all;
use rspotify::{
    model::{PlayableItem, PlaylistId, SavedTrack, TrackId, UserId},
    prelude::*,
    scopes, AuthCodeSpotify, Credentials, OAuth,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    error::Error,
    fs::{create_dir_all, File},
    io::{self, BufReader, Read, Write},
    path::Path,
    time::Duration,
};
use tokio::{spawn, time::sleep};

#[derive(Serialize, Deserialize, Debug)]
struct TimeRating {
    added_at: DateTime<Utc>,
    rating: f32,
}

impl TimeRating {
    fn new(added_at: DateTime<Utc>, rating: f32) -> Self {
        Self { added_at, rating }
    }
}

const DEFAULT_RATING_DB_PATH: &str = "spotility/ratings.json";
fn cli() -> Command {
    Command::new("spotility")
        .about("A CLI for managing your 'Liked Songs'")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("top")
                .about("Extracts the newest 'Liked Songs' into a new Playlist")
                .arg(arg!(<AMOUNT> "Amount of songs to extract").value_parser(value_parser!(u32))).arg_required_else_help(true)
                .arg(arg!(<USERNAME> "Spotify API username").long("username").env("SPOTIFY_API_USERNAME")).arg_required_else_help(true)
                .arg(arg!(--name <NAME> "Name of the playlist").id("NAME"))
                // spotify api authentification
                .arg(arg!(<ID> "Spotify API authentification ID").long("id").env("SPOTIFY_API_ID"))
                .arg(arg!(<SECRET> "Spotify API authentification secret").long("secret").env("SPOTIFY_API_SECRET"))
        )
        .subcommand(
            Command::new("rate")
                .about("Rates the currently playing song (For use with the weights command)")
                .arg(arg!(<RATING> "Rating to apply")).arg_required_else_help(true)
                .arg(arg!([DB_PATH] "The path of the rating database").long("db_path").default_value(DEFAULT_RATING_DB_PATH))
                // spotify api authentification
                .arg(arg!(<ID> "Spotify API authentification ID").long("id").env("SPOTIFY_API_ID"))
                .arg(arg!(<SECRET> "Spotify API authentification secret").long("secret").env("SPOTIFY_API_SECRET"))
                .arg(arg!(--ask "Asks for confirmation for the right song").action(ArgAction::SetTrue).id("ASK"))
        )
        .subcommand(
            Command::new("weights")
                .about("Updates/Creates the user playlist 'Liked Songs' and generates weights for use with weighting spotify plugin")
                .arg(arg!([DB_PATH] "The path of the rating database").long("db_path").default_value(DEFAULT_RATING_DB_PATH))
                .arg(arg!(--"output-file" <PATH> "Print the weighths to the stdOut").id("PATH"))
        )
        .subcommand(
            Command::new("update-db")
                .about("Updates the rating database")
                .arg(arg!([LIMIT] "Up until when the db should be updated").long("limit").default_value("50").value_parser(value_parser!(u32)))
                .arg(arg!([DB_PATH] "The path of the rating database").long("db_path").default_value(DEFAULT_RATING_DB_PATH))
                // spotify api authentification
                .arg(arg!(<ID> "Spotify API authentification ID").long("id").env("SPOTIFY_API_ID"))
                .arg(arg!(<SECRET> "Spotify API authentification secret").long("secret").env("SPOTIFY_API_SECRET"))
        )
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let matches = cli().get_matches();

    match matches.subcommand() {
        Some(("top", sub_matches)) => {
            // api authentification
            let id = sub_matches.get_one::<String>("ID").expect("ID is required");
            let secret = sub_matches
                .get_one::<String>("SECRET")
                .expect("SECRET is required");
            let spotify = authenticate(id, secret).await;

            let amount = sub_matches
                .get_one::<u32>("AMOUNT")
                .expect("amount is required");
            let username = sub_matches
                .get_one::<String>("USERNAME")
                .expect("username is required");
            let playlist_name = match sub_matches.get_one::<String>("NAME") {
                Some(playlist_name) => playlist_name.clone(),
                None => format!("Top {amount}"),
            };

            // get track id's
            let liked_songs_ids = get_liked_songs(spotify.clone(), *amount)
                .await
                .unwrap()
                .into_iter()
                .map(|saved_track| saved_track.track.id.unwrap())
                .collect();

            // search/create playlist with correct name
            let playlist_id = empty_playlist(spotify.clone(), username.clone(), playlist_name)
                .await
                .unwrap();

            // replace songs in playlist
            populate_playlist(spotify, playlist_id, liked_songs_ids)
                .await
                .unwrap();
        }
        Some(("weights", sub_matches)) => {
            // get db path
            let db_path = sub_matches
                .get_one::<String>("DB_PATH")
                .expect("db-path has default value");

            // get ratings db
            let ratings = match load_hashmap(db_path.clone()) {
                Ok(hashmap) => hashmap,
                Err(e) => {
                    println!("Error loading database: {e}");
                    return Ok(());
                }
            };

            let mut ratings_vec: Vec<_> = ratings.into_iter().collect();
            // sort the vec by time added (unstable, because faster)
            ratings_vec.sort_by(|a, b| b.1.added_at.cmp(&a.1.added_at));
            ratings_vec.sort_by(|a, b| {
                a.1.rating
                    .partial_cmp(&b.1.rating)
                    .expect("all elements have a (non NaN) rating")
            });

            let ratings_len = ratings_vec.len();
            let step = 10.0 / ratings_len as f64;

            // creating weights
            println!("Creating weights");
            let weights = ratings_vec
                .into_iter()
                .enumerate()
                .map(|(i, (song_id, _))| {
                    let rating = 9.0 - (step * i as f64) + 1.0;
                    format!("{}:{:.2}", song_id, rating)
                })
                .collect::<Vec<String>>()
                .join("|");

            match sub_matches.get_one::<String>("PATH") {
                Some(path) => {
                    // writing weights to file
                    println!("Writing weights to file");
                    let mut file = File::create(path)?;
                    file.write_all(weights.as_bytes())?;
                }
                None => {
                    // copying weights to clipboard
                    println!("Copying weights to clipboard");
                    let mut clipboard: ClipboardContext = ClipboardProvider::new()?;
                    clipboard.set_contents(weights.clone())?;
                }
            }
        }
        Some(("rate", sub_matches)) => {
            // api authentification
            let id = sub_matches.get_one::<String>("ID").expect("ID is required");
            let secret = sub_matches
                .get_one::<String>("SECRET")
                .expect("SECRET is required");
            let spotify = authenticate(id, secret).await;

            // get rating
            let rating = match sub_matches
                .get_one::<String>("RATING")
                .expect("rating is required") as &str
            {
                "great" | "1" => 1.,
                "good" | "2" => 2.,
                "ok" | "3" => 3.,
                "bad" | "4" => 4.,
                other => other.parse::<f32>().unwrap(),
            };
            // get db path
            let db_path = sub_matches
                .get_one::<String>("DB_PATH")
                .expect("db-path has default value");

            // get currently playing song
            let currently_playing_song = match spotify.current_user_playing_item().await? {
                Some(currently_playing_context) => currently_playing_context.item.unwrap(),
                None => {
                    println!("No currently playing song");
                    return Ok(());
                }
            };

            match sub_matches.get_flag("ASK") {
                false => {
                    // print currently rating song
                    println!(
                        "Rating song {}",
                        match currently_playing_song {
                            PlayableItem::Track(ref full_track) => full_track.name.clone(),
                            PlayableItem::Episode(ref full_episode) => full_episode.name.clone(),
                        }
                    );
                }
                true => {
                    // print currently rating song and get user confirmation
                    println!(
                        "Rating song {} -- Continue? y/N",
                        match currently_playing_song {
                            PlayableItem::Track(ref full_track) => full_track.name.clone(),
                            PlayableItem::Episode(ref full_episode) => full_episode.name.clone(),
                        }
                    );
                    // read input
                    let mut confirmation_buffer = String::new();
                    io::stdin().read_line(&mut confirmation_buffer)?;
                    // end the program if not 'y'
                    match &confirmation_buffer as &str {
                        "y" | "Y" | "yes" | "Yes" => {}
                        _ => return Ok(()),
                    }
                }
            }

            // get ratings db
            let mut ratings = match load_hashmap(db_path.clone()) {
                Ok(hashmap) => hashmap,
                Err(e) => {
                    println!("Error loading database: {e}");
                    return Ok(());
                }
            };

            // print change
            println!(
                "{} -> {}",
                make_readable(
                    match ratings.get(currently_playing_song.id().unwrap().id()) {
                        // Print rating if song is found
                        Some(time_rating) => time_rating.rating,
                        // Print error message and exit if song is not found
                        None => {
                            println!("Error fetching song from local database.");
                            return Ok(());
                        }
                    }
                ),
                make_readable(rating)
            );

            // apply change
            ratings
                .get_mut(currently_playing_song.id().unwrap().id())
                .unwrap()
                .rating = rating;

            save_hashmap(db_path.clone(), &ratings)?;
        }
        Some(("update-db", sub_matches)) => {
            // api authentification
            let id = sub_matches.get_one::<String>("ID").expect("ID is required");
            let secret = sub_matches
                .get_one::<String>("SECRET")
                .expect("SECRET is required");
            let spotify = authenticate(id, secret).await;

            // get limit
            let limit = sub_matches
                .get_one::<u32>("LIMIT")
                .expect("limit is required");
            // get db_path
            let db_path = sub_matches
                .get_one::<String>("DB_PATH")
                .expect("db_path is required");

            // get liked songs up until the limit
            let liked_songs_to_limit = get_liked_songs(spotify, *limit).await.unwrap();

            // get ratings db
            let mut ratings = load_or_create_hashmap(db_path.clone())?;

            if ratings.is_empty() {
                println!("No local database, creating new one");
            }

            for liked_song in liked_songs_to_limit {
                let _ = ratings
                    .entry(liked_song.track.id.unwrap().id().to_string())
                    .or_insert(TimeRating::new(liked_song.added_at, 3.));
            }

            save_hashmap(db_path.clone(), &ratings)?;
        }
        _ => unreachable!(), // All subcommands listed
    };

    Ok(())
}

fn make_readable(input: f32) -> String {
    match input {
        1.0 => "great".to_string(),
        2.0 => "good".to_string(),
        3.0 => "ok".to_string(),
        4.0 => "bad".to_string(),
        other => other.to_string(),
    }
}

fn load_or_create_hashmap(
    file_path: String,
) -> Result<HashMap<String, TimeRating>, Box<dyn Error>> {
    load_hashmap(file_path).map_or(Ok(HashMap::new()), Ok)
}

fn load_hashmap(file_path: String) -> Result<HashMap<String, TimeRating>, Box<dyn Error>> {
    match File::open(file_path) {
        Ok(file) => {
            // If the file exists, attempt to read from it
            let mut file_contents = String::new();
            let mut buf_reader = BufReader::new(file);
            buf_reader.read_to_string(&mut file_contents)?;
            // Deserialize JSON to HashMap
            Ok(serde_json::from_str(&file_contents)?)
        }
        // If the file doesnt exist, create a new HashMap
        Err(e) => Err(Box::new(e)),
    }
}

fn save_hashmap(
    file_path: String,
    hashmap: &HashMap<String, TimeRating>,
) -> Result<(), Box<dyn Error>> {
    let serialized_hashmap = serde_json::to_string(hashmap)?;

    // Extract the parent directory from the provided file path
    if let Some(parent) = Path::new(&file_path).parent() {
        create_dir_all(parent)?; // Create the directory structure if it does not exist
    }

    let mut file = File::create(file_path)?;

    file.write_all(serialized_hashmap.as_bytes())?;

    Ok(())
}

async fn authenticate(id: &str, secret: &str) -> AuthCodeSpotify {
    let creds = Credentials::new(id, secret);

    let oauth = OAuth {
        redirect_uri: "http://localhost:8888/callback/".to_string(),
        scopes: scopes!("playlist-modify-public playlist-modify-private user-library-read playlist-read-private user-read-currently-playing"),
        ..Default::default()
    };

    let spotify = AuthCodeSpotify::new(creds, oauth);

    let url = spotify.get_authorize_url(false).unwrap();
    spotify.prompt_for_token(&url).await.unwrap();

    spotify
}

async fn get_liked_songs(
    spotify: AuthCodeSpotify,
    amount: u32,
) -> Result<Vec<SavedTrack>, Box<dyn Error + Send>> {
    let batch_size = 50;
    let full_batches = amount / batch_size;
    // size of the last batch
    let final_batch_size = amount % batch_size;
    // basically (amount / batch_size) + .1
    let batches_amount = if final_batch_size > 0 {
        full_batches + 1
    } else {
        full_batches
    };

    let tasks = (0..batches_amount).map(|i| {
        let offset = i * batch_size;
        let spotify_clone = spotify.clone();
        let current_batch_size = if i == full_batches && final_batch_size > 0 {
            final_batch_size
        } else {
            batch_size
        };

        spawn(async move {
            Ok::<Vec<SavedTrack>, Box<dyn Error + Send>>(
                spotify_clone
                    .current_user_saved_tracks_manual(None, Some(current_batch_size), Some(offset))
                    .await
                    // make error 'Send'
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send>)?
                    .items,
            )
        })
    });

    let mut all_tracks = Vec::new();
    for task in join_all(tasks).await {
        all_tracks.extend(task.map_err(|e| Box::new(e) as Box<dyn Error + Send>)??);
    }

    Ok(all_tracks)
}

async fn search_for_playlist(
    spotify: AuthCodeSpotify,
    playlist_name: String,
) -> Result<Option<PlaylistId<'static>>, Box<dyn Error + Send>> {
    // currently existing playlists
    let existing_playlists = spotify
        .current_user_playlists_manual(Some(50), None)
        .await
        // make the error 'Send'
        .map_err(|e| Box::new(e) as Box<dyn Error + Send>)?;

    Ok(existing_playlists
        .items
        .into_iter()
        .find(|existing_playlist| existing_playlist.name == playlist_name)
        // get playlist id
        .map(|playlist| playlist.id))
}

async fn create_playlist(
    spotify: AuthCodeSpotify,
    username: String,
    playlist_name: String,
) -> Result<PlaylistId<'static>, Box<dyn Error + Send>> {
    Ok(spotify
        // create playlist
        .user_playlist_create(
            UserId::from_id(username).expect("Expected username to be valid"),
            &playlist_name,
            Some(false),
            Some(false),
            None,
        )
        .await
        // make the error 'Send'
        .map_err(|e| Box::new(e) as Box<dyn Error + Send>)?
        // get id
        .id)
}

/// Removes items if playlist already exists, creates playlist if not
async fn empty_playlist(
    spotify: AuthCodeSpotify,
    username: String,
    playlist_name: String,
) -> Result<PlaylistId<'static>, Box<dyn Error + Send>> {
    // get playlist
    let searched_playlist = search_for_playlist(spotify.clone(), playlist_name.clone()).await?;

    // empty vec, to clear playlist
    let empty_items: Vec<PlayableId<'static>> = Vec::new();
    match searched_playlist {
        Some(playlist_id) => {
            spotify
                .playlist_replace_items(playlist_id.clone(), empty_items)
                .await
                // make error 'Send'
                .map_err(|e| Box::new(e) as Box<dyn Error + Send>)?;
            Ok(playlist_id)
        }
        None => Ok(create_playlist(spotify, username, playlist_name).await?),
    }
}

/// Populates the given playlist with the given song id's
async fn populate_playlist(
    spotify: AuthCodeSpotify,
    playlist_id: PlaylistId<'static>,
    song_ids: Vec<TrackId<'static>>,
) -> Result<(), Box<dyn Error + Send>> {
    // Given by the spotify API docs
    let batch_size = 100;

    let max_retries = 3;
    let delay_between_retries = Duration::from_secs(2);

    let tasks: Vec<_> = song_ids
        .chunks(batch_size)
        .map(|chunk| {
            let spotify_clone = spotify.clone();
            let playlist_id_clone = playlist_id.clone();

            // make a owned version of the chunk
            let chunk_owned = chunk.to_vec();

            async move {
                let mut retries = 0;
                loop {
                    let chunk_vec = chunk_owned
                        .iter()
                        .map(|track_id| PlayableId::Track(track_id.clone()))
                        .collect::<Vec<_>>();
                    match spotify_clone
                        .playlist_add_items(playlist_id_clone.clone(), chunk_vec, None)
                        .await
                    {
                        Ok(_) => return Ok(()),
                        Err(e) if retries < max_retries => {
                            println!("Retrying due to error: {}", e);
                            retries += 1;
                            sleep(delay_between_retries).await;
                        }
                        Err(e) => return Err(Box::new(e) as Box<dyn Error + Send>),
                    }
                }
            }
        })
        .collect();

    let results = join_all(tasks).await;
    for result in results {
        match result {
            Ok(()) => {
                // If the task succeeded, you can process the successful addition here.
            }
            Err(e) => {
                println!("Failed to add items to the playlist: {e}");
            }
        }
    }

    Ok(())
}

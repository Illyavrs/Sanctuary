use crate::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use std::str::FromStr;
use tower::ServiceExt;
const MAP: &str = "osu file format v14\n[General]\nMode:0\n[Metadata]\nTitle:Integration fixture\nArtist:Illyaverse\nCreator:Test\nVersion:Easy\n[Difficulty]\nHPDrainRate:5\nCircleSize:4\nOverallDifficulty:5\nApproachRate:5\nSliderMultiplier:1.4\nSliderTickRate:1\n[TimingPoints]\n0,500,4,2,1,100,1,0\n[HitObjects]\n64,64,1000,1,0,0:0:0:0:\n256,192,2000,1,0,0:0:0:0:\n";
async fn register(app: &Router, name: &str) -> (i64, String) {
    let response=app.clone().oneshot(Request::builder().method("POST").uri("/api/v1/auth/register").header(header::ORIGIN,"http://localhost:3000").header(header::CONTENT_TYPE,"application/json").body(Body::from(json!({"username":name,"email":format!("{name}@example.invalid"),"password":"fixture-password-1234"}).to_string())).unwrap()).await.unwrap();
    assert!(response.status().is_success());
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let value: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_048_576).await.unwrap()).unwrap();
    (value["user"]["id"].as_i64().unwrap(), cookie)
}

fn replay(user: i64, name: &str, checksum: &str, legacy: bool, ticks: i64) -> Vec<u8> {
    let submission = serde_json::from_value(json!({"ruleset_id":0,"passed":true,"total_score":1234,"total_score_without_mods":1234,"accuracy":1.0,"max_combo":2,"rank":"X","mods":[],"statistics":{"great":2},"maximum_statistics":{"great":2},"pauses":[]})).unwrap();
    let binding = resonance_protocol::lazer_replay::ReplayScore {
        online_id: 42,
        user_id: user,
        username: name,
        beatmap_md5: checksum,
        client_version: "fixture",
        mods_bits: 0,
        timestamp_ticks: ticks,
        submission: &submission,
    };
    let bytes = resonance_protocol::lazer_replay::encode(
        &binding,
        b"0|256|192|0,1000|64|64|1,1000|256|192|1,-12345|0|0|0",
    )
    .unwrap();
    if !legacy {
        return bytes;
    }
    let parsed = resonance_protocol::lazer_replay::ExtendedReplay::parse(&bytes).unwrap();
    let mut writer = resonance_protocol::stable::Writer::new();
    writer
        .u8(0)
        .i32(20261012)
        .string(checksum)
        .unwrap()
        .string(name)
        .unwrap()
        .string("")
        .unwrap();
    for count in [2, 0, 0, 0, 0, 0] {
        writer.u16(count);
    }
    writer
        .i32(1234)
        .u16(2)
        .boolean(true)
        .u32(0)
        .string("")
        .unwrap()
        .i64(ticks)
        .i32(parsed.legacy.compressed_frames.len() as i32);
    let mut result = writer.into_bytes();
    result.extend(parsed.legacy.compressed_frames);
    result.extend(42i64.to_le_bytes());
    result
}
async fn submit(
    app: &Router,
    cookie: Option<&str>,
    bytes: &[u8],
    extra: &str,
) -> (StatusCode, Value) {
    let mut body=b"--review-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.osr\"\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
    body.extend(bytes);
    body.extend(
        b"\r\n--review-test\r\nContent-Disposition: form-data; name=\"passed\"\r\n\r\ntrue\r\n",
    );
    body.extend(extra.as_bytes());
    body.extend(b"--review-test--\r\n");
    request(
        app,
        "POST",
        "/api/v1/scores/resubmit",
        cookie,
        body,
        "multipart/form-data; boundary=review-test",
    )
    .await
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Vec<u8>,
    content: &str,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::ORIGIN, "http://localhost:3000")
        .header(header::CONTENT_TYPE, content);
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[test]
fn summaries_are_bounded_and_do_not_prove_gameplay() {
    let bytes = replay(
        7,
        "ReviewOwner",
        "0123456789abcdef0123456789abcdef",
        true,
        639269732961234567,
    );
    let result = super::parse(&bytes, false).unwrap();
    assert_eq!(result.client, "stable");
    assert!(!result.input.passed);
    assert_eq!(result.input.accuracy, 100.0);
    let bytes = replay(
        7,
        "ReviewOwner",
        "0123456789abcdef0123456789abcdef",
        false,
        639269732961234567,
    );
    let result = super::parse(&bytes, false).unwrap();
    assert_eq!(result.client, "lazer");
    assert!(result.input.passed);
    assert_eq!(result.native_user, Some(7));
    assert!(super::parse(b"invalid", true).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(super::parse(&trailing, true).is_err());
}
#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; fresh PostgreSQL schema per run"]
async fn replay_claims_need_review_and_preserve_ownership_and_retries() {
    let url = env::var("ILLYA_TEST_DATABASE_URL").unwrap();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("replay_resubmission_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&control)
        .await
        .unwrap();
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let db = PgPoolOptions::new()
        .max_connections(6)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&db).await.unwrap();
    let directory = std::env::temp_dir().join(&schema);
    tokio::fs::create_dir_all(directory.join("replays"))
        .await
        .unwrap();
    let state = AppState {
        db: db.clone(),
        secret: Arc::new("test-session-secret-with-at-least-thirty-two-bytes".into()),
        internal_key: Arc::new("test-internal-secret-with-at-least-thirty-two-bytes".into()),
        origin: Arc::new("http://localhost:3000".into()),
        mail: Arc::new(mail_delivery::MailConfig::disabled()),
        secure_cookie: false,
        media_root: directory.join("media"),
        maps_root: directory.join("maps"),
        replay_root: directory.join("replays"),
        client: reqwest::Client::new(),
        official: Arc::new(Mutex::new(maps::OfficialState::default())),
        limits: Default::default(),
        trusted_proxies: Default::default(),
        upload_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        chat_runtime: Default::default(),
    };

    tokio::fs::create_dir_all(directory.join("maps"))
        .await
        .unwrap();
    tokio::fs::write(directory.join("maps/fixture.osu"), MAP)
        .await
        .unwrap();
    let app = router(state);
    let app_clone = app.clone();
    let pool = db.clone();
    let files = directory.clone();
    let result=tokio::spawn(async move {
        let (owner,cookie)=register(&app_clone,"ReviewOwner").await;
        let (_,stranger)=register(&app_clone,"ReviewStranger").await;
        let (moderator,staff)=register(&app_clone,"ReviewStaff").await;
        sqlx::query("INSERT INTO user_roles(user_id,role_id) VALUES($1,2)").bind(moderator).execute(&pool).await.unwrap();
        let checksum=format!("{:x}",md5::compute(MAP));
        let map:i64=sqlx::query_scalar("INSERT INTO beatmaps(title,artist,creator,version,mode,status,stars,max_combo,md5,osu_path) VALUES('Fixture','Illyaverse','Tests','Easy',0,'ranked',1,2,$1,'fixture.osu') RETURNING id").bind(&checksum).fetch_one(&pool).await.unwrap();
        let bytes=replay(owner,"ReviewOwner",&checksum,true,639269732961234567);
        assert_eq!(submit(&app_clone,None,&bytes,"").await.0,StatusCode::UNAUTHORIZED);
        let wrong=submit(&app_clone,Some(&stranger),&bytes,"").await;assert_eq!(wrong.0,StatusCode::BAD_REQUEST);assert_eq!(wrong.1["error"]["code"],"replay_owner");
        assert_eq!(submit(&app_clone,Some(&cookie),b"bad","").await.0,StatusCode::BAD_REQUEST);
        assert_eq!(submit(&app_clone,Some(&cookie),&bytes,"--review-test\r\nContent-Disposition: form-data; name=\"verification\"\r\n\r\nverified\r\n").await.0,StatusCode::BAD_REQUEST);
        let (left,right)=tokio::join!(submit(&app_clone,Some(&cookie),&bytes,""),submit(&app_clone,Some(&cookie),&bytes,""));
        assert_eq!(left.0,StatusCode::OK,"{}",left.1);assert_eq!(right.0,StatusCode::OK,"{}",right.1);assert_eq!(left.1["id"],right.1["id"]);
        let id=left.1["id"].as_i64().unwrap();assert_eq!(left.1["verification"],"pending");
        let record:(String,String,Option<bool>)=sqlx::query_as("SELECT verification,submission_origin,replay_complete FROM scores WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();assert_eq!(record,("pending".into(),"replay".into(),None));
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM scores WHERE user_id=$1 AND verification='verified'").bind(owner).fetch_one(&pool).await.unwrap();assert_eq!(count,0);
        assert_eq!(request(&app_clone,"GET",&format!("/api/v1/scores/{id}"),None,vec![],"application/json").await.0,StatusCode::NOT_FOUND);
        assert_eq!(request(&app_clone,"GET",&format!("/api/v1/scores/{id}/replay"),Some(&stranger),vec![],"application/json").await.0,StatusCode::FORBIDDEN);
        let detail=request(&app_clone,"GET",&format!("/api/v1/scores/{id}"),Some(&cookie),vec![],"application/json").await;assert_eq!(detail.1["submission_origin"],"replay");assert_eq!(detail.1["accuracy_source"],"replay_summary");assert_eq!(detail.1["has_replay"],true);
        let decision=json!({"verification":"verified","expected_revision":0}).to_string().into_bytes();
        assert_eq!(request(&app_clone,"PATCH",&format!("/api/v1/admin/scores/{id}"),Some(&cookie),decision.clone(),"application/json").await.0,StatusCode::FORBIDDEN);
        let reviewed=request(&app_clone,"PATCH",&format!("/api/v1/admin/scores/{id}"),Some(&staff),decision,"application/json").await;assert_eq!(reviewed.0,StatusCode::OK,"{}",reviewed.1);
        let retry=submit(&app_clone,Some(&cookie),&bytes,"").await;assert_eq!(retry.1["id"],id);assert_eq!(retry.1["verification"],"verified");assert_eq!(retry.1["idempotent"],true);
        assert_eq!(request(&app_clone,"GET",&format!("/api/v1/scores/{id}"),None,vec![],"application/json").await.0,StatusCode::OK);
        let rejected=request(&app_clone,"PATCH",&format!("/api/v1/admin/scores/{id}"),Some(&staff),json!({"verification":"rejected","expected_revision":1}).to_string().into_bytes(),"application/json").await;
        assert_eq!(rejected.0,StatusCode::OK);
        let retry=submit(&app_clone,Some(&cookie),&bytes,"").await;assert_eq!(retry.1["verification"],"rejected");
        let native=replay(owner,"ReviewOwner",&checksum,false,639269732971234567);let new=submit(&app_clone,Some(&cookie),&native,"").await;assert_eq!(new.0,StatusCode::OK,"{}",new.1);assert_eq!(new.1["verification"],"pending");
        let mismatch=replay(owner+99,"ReviewOwner",&checksum,false,639269732981234567);assert_eq!(submit(&app_clone,Some(&cookie),&mismatch,"").await.0,StatusCode::BAD_REQUEST);
        let unknown=replay(owner,"ReviewOwner","0123456789abcdef0123456789abcdef",true,639269732991234567);let badmap=submit(&app_clone,Some(&cookie),&unknown,"").await;assert_eq!(badmap.1["error"]["code"],"replay_map");
        // Audit failure rolls back both claim and the owned replay file.
        sqlx::query("CREATE FUNCTION fail_replay_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='scores.replay_submit' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$").execute(&pool).await.unwrap();
        sqlx::query("CREATE TRIGGER fail_replay_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_replay_audit()").execute(&pool).await.unwrap();
        let another=replay(owner,"ReviewOwner",&checksum,true,639269733001234567);assert_eq!(submit(&app_clone,Some(&cookie),&another,"").await.0,StatusCode::INTERNAL_SERVER_ERROR);
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM scores WHERE user_id=$1").bind(owner).fetch_one(&pool).await.unwrap();assert_eq!(count,2);
        let mut entries=tokio::fs::read_dir(files.join("replays")).await.unwrap();let mut files_count=0;while entries.next_entry().await.unwrap().is_some(){ files_count+=1; }assert_eq!(files_count,2);
        sqlx::query("DROP TRIGGER fail_replay_audit ON audit_events").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO scores(user_id,beatmap_id,mode,score,accuracy,max_combo,pp,passed,submission_origin) SELECT $1,$2,0,1234,100,2,0,true,'replay' FROM generate_series(1,19)").bind(owner).bind(map).execute(&pool).await.unwrap();
        let limited=submit(&app_clone,Some(&cookie),&another,"").await;assert_eq!(limited.0,StatusCode::TOO_MANY_REQUESTS);assert_eq!(limited.1["error"]["code"],"replay_review_limit");
        let retry=submit(&app_clone,Some(&cookie),&bytes,"").await;assert_eq!(retry.1["id"],id);assert_eq!(retry.1["verification"],"rejected");
        sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(owner).execute(&pool).await.unwrap();
        assert!(matches!(submit(&app_clone,Some(&cookie),&another,"").await.0,StatusCode::UNAUTHORIZED|StatusCode::FORBIDDEN));
        let _=map;
    }).await;
    db.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&control)
        .await
        .unwrap();
    tokio::fs::remove_dir_all(directory).await.unwrap();
    result.unwrap();
}

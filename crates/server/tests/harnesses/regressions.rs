use super::*;

fn last_request(h: &Harness) -> Value {
    h.requests.lock().unwrap().last().unwrap().0.clone()
}

fn collaboration_mode(body: &Value) -> String {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|content| content["text"].as_str())
        // The default instructions themselves quote this tag. Read the outer
        // opening tag, then select the most recent developer message.
        .filter_map(|text| text.split_once("<collaboration_mode>"))
        .next_back()
        .map(|(_, mode)| mode.to_owned())
        .unwrap_or_default()
}

#[tokio::test]
#[ignore = "requires actual Codex and Claude SDK in the harness image"]
async fn native_settings_reach_the_next_model_request() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let outcome = async {
        for harness in ["claude", "codex"] {
            let conv = h
                .configured(
                    &user,
                    "work",
                    json!({"harness":harness,"plan":harness=="codex"}),
                    None,
                )
                .await?;
            h.submit(&user, conv, message("initial settings")).await?;
            h.wait_state(conv, "completed").await?;
            h.idle(conv).await?;
            let first = last_request(&h);
            if harness == "claude" {
                ensure!(first["output_config"]["effort"] == "medium");
            } else {
                ensure!(collaboration_mode(&first).contains("# Plan Mode"));
            }
            for reasoning in ["high", "low"] {
                h.submit(
                    &user,
                    conv,
                    Command::UpdateRunConfig(UpdateRunConfig {
                        config_json: json!({"reasoning":reasoning,"plan":false}).to_string(),
                    }),
                )
                .await?;
                h.submit(&user, conv, message("use the updated settings"))
                    .await?;
                h.wait_state(conv, "completed").await?;
                h.idle(conv).await?;
                let request = last_request(&h);
                if harness == "claude" {
                    ensure!(
                        request["output_config"]["effort"] == reasoning,
                        "Claude effort did not change: {}",
                        request["output_config"]
                    );
                } else {
                    ensure!(request["reasoning"]["effort"] == reasoning);
                    ensure!(
                        collaboration_mode(&request).contains("You are now in Default mode."),
                        "Codex did not exit Plan Mode: {:?}",
                        collaboration_mode(&request)
                    );
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

async fn wait_for_channel_locks(db: &PgPool, count: i64) -> anyhow::Result<()> {
    for _ in 0..100 {
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT id FROM channels%'").fetch_one(db).await?;
        if waiting >= count {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("channel lock barrier not reached");
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn concurrent_project_delete_and_move_remain_recoverable() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let outcome = async {
        let project = h
            .json(
                &user,
                &format!("/orgs/{org}/projects"),
                reqwest::Method::POST,
                json!({"name":"Concurrent deletion"}),
            )
            .await?;
        let project = id(&project, "id");
        let conv = h
            .configured(&user, "chat", json!({}), Some(project))
            .await?;
        let mut blocker = h.db.begin().await?;
        sqlx::query("SELECT id FROM channels WHERE conversation_id=$1 FOR UPDATE")
            .bind(conv)
            .fetch_one(&mut *blocker)
            .await?;
        let client = reqwest::Client::new();
        let deleting = tokio::spawn(
            client
                .delete(format!("{}/orgs/{org}/projects/{project}", h.base))
                .bearer_auth(user["token"].as_str().unwrap())
                .send(),
        );
        wait_for_channel_locks(&h.db, 1).await?;
        let moving = tokio::spawn(
            client
                .post(format!(
                    "{}/orgs/{org}/conversations/{conv}/project",
                    h.base
                ))
                .bearer_auth(user["token"].as_str().unwrap())
                .json(&json!({"project_id":null}))
                .send(),
        );
        wait_for_channel_locks(&h.db, 2).await?;
        blocker.commit().await?;
        ensure!(deleting.await??.status().is_success());
        ensure!(moving.await??.status() == reqwest::StatusCode::FORBIDDEN);
        let row: (String, Option<Uuid>) =
            sqlx::query_as("SELECT state,project_id FROM conversations WHERE id=$1")
                .bind(conv)
                .fetch_one(&h.db)
                .await?;
        ensure!(row == ("deletion_pending".into(), Some(project)));
        h.json(
            &user,
            &format!("/orgs/{org}/projects/{project}/restore"),
            reqwest::Method::POST,
            json!({}),
        )
        .await?;
        let state: String = sqlx::query_scalar("SELECT state FROM conversations WHERE id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
        ensure!(
            state == "active",
            "the conversation must be restored with its project"
        );
        h.json(
            &user,
            &format!("/orgs/{org}/conversations/{conv}/project"),
            reqwest::Method::POST,
            json!({"project_id":null}),
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires actual Codex in the harness image"]
async fn codex_forks_only_at_completed_turn_boundaries() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    let outcome = async {
        let conv = h
            .configured(&user, "work", json!({"harness":"codex"}), None)
            .await?;
        h.submit(&user, conv, message("HARNESS_FORK_BOUNDARY")).await?;
        let run = h.wait_state(conv, "requires_action").await?;
        let (action, epoch) = h.action(run).await?;
        h.submit(
            &user,
            conv,
            Command::DecideApproval(DecideApproval {
                action_id: action.to_string(),
                run_epoch: epoch as u64,
                decision: "allow".into(),
            }),
        )
        .await?;
        h.wait_state(conv, "completed").await?;
        h.idle(conv).await?;
        let early: Uuid = sqlx::query_scalar(
            "SELECT id FROM items WHERE run_id=$1 AND kind='assistant' AND text='HARNESS_BEFORE_TOOL'",
        )
        .bind(run)
        .fetch_one(&h.db)
        .await?;
        let final_reply: Uuid = sqlx::query_scalar(
            "SELECT id FROM items WHERE run_id=$1 AND kind='assistant' AND text='HARNESS_AFTER_TOOL'",
        )
        .bind(run)
        .fetch_one(&h.db)
        .await?;
        let catalog = h
            .json(
                &user,
                &format!("/orgs/{org}/conversations/{conv}/commands"),
                reqwest::Method::GET,
                json!({}),
            )
            .await?;
        let points = catalog["fork_points"].as_array().unwrap();
        ensure!(!points.contains(&json!(early)) && points.contains(&json!(final_reply)));
        let error = h
            .submit(
                &user,
                conv,
                Command::Fork(Fork {
                    item_id: early.to_string(),
                }),
            )
            .await
            .unwrap_err();
        ensure!(
            error.to_string().contains("FORK_POINT_UNAVAILABLE"),
            "{error}"
        );
        h.submit(&user, conv, message("HARNESS_FUTURE_CONTEXT")).await?;
        h.wait_state(conv, "completed").await?;
        h.idle(conv).await?;
        let fork = h
            .submit(
                &user,
                conv,
                Command::Fork(Fork {
                    item_id: final_reply.to_string(),
                }),
            )
            .await?;
        let fork = h.fork_result(fork).await?;
        h.submit(&user, fork, message("HARNESS_CHILD_CONTEXT")).await?;
        h.wait_state(fork, "completed").await?;
        h.idle(fork).await?;
        let request = last_request(&h).to_string();
        ensure!(
            request.contains("HARNESS_BEFORE_TOOL")
                && request.contains("HARNESS_AFTER_TOOL")
                && request.contains("HARNESS_TOOL_RESULT")
        );
        ensure!(
            !request.contains("HARNESS_FUTURE_CONTEXT"),
            "fork included a later turn"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn conversation_metadata_replays_to_online_and_offline_clients() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let outcome = async {
        let ctx = h.app.store.context(id(&user, "user_id"), org).await?;
        let conv = h.configured(&user, "chat", json!({}), None).await?;
        let channel = format!("user:{org}:{}", id(&user, "user_id"));
        let mut online = ryzhub_core::Engine::default();
        online.subscribe(channel.clone());
        for frame in h.app.store.replay(&ctx, &channel, None).await?.0 {
            online.apply(frame);
        }
        let mut offline = online.clone();
        let mut revision = online.channels[&channel].items[&conv.to_string()].revision;
        for title in ["first rename", "second rename"] {
            h.json(
                &user,
                &format!("/orgs/{org}/conversations/{conv}/settings"),
                reqwest::Method::PATCH,
                json!({"title":title,"archive":true}),
            )
            .await?;
            let (frames, _) = h
                .app
                .store
                .replay(&ctx, &channel, Some(online.channels[&channel].cursor))
                .await?;
            ensure!(frames.len() == 1, "metadata must produce one durable event");
            for frame in frames {
                online.apply(frame);
            }
            let item = &online.channels[&channel].items[&conv.to_string()];
            ensure!(item.text == title && item.state == "archived" && item.revision > revision);
            revision = item.revision;
        }
        let (frames, _) = h
            .app
            .store
            .replay(&ctx, &channel, Some(offline.channels[&channel].cursor))
            .await?;
        ensure!(frames.len() == 2);
        for frame in frames {
            offline.apply(frame);
        }
        ensure!(offline.channels[&channel].items == online.channels[&channel].items);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn a_pin_stays_listed_and_reaches_every_client() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let outcome = async {
        let ctx = h.app.store.context(id(&user, "user_id"), org).await?;
        let older = h.configured(&user, "chat", json!({}), None).await?;
        let newer = h
            .configured(&user, "work", json!({"harness":"codex"}), None)
            .await?;
        let channel = format!("user:{org}:{}", id(&user, "user_id"));
        let order = || async {
            let list = h
                .json(
                    &user,
                    &format!("/orgs/{org}/conversations"),
                    reqwest::Method::GET,
                    json!({}),
                )
                .await?;
            anyhow::Ok(list.as_array().context("list")?.clone())
        };
        let pinned_at = |client: &ryzhub_core::Engine, conv: Uuid| {
            let item = &client.channels[&channel].items[&conv.to_string()];
            match item.detail.as_ref().and_then(|d| d.kind.as_ref()) {
                Some(item_detail::Kind::Conversation(c)) => Ok(c.pinned_at_ms),
                other => Err(anyhow::anyhow!("no conversation detail: {other:?}")),
            }
        };
        let mut client = ryzhub_core::Engine::default();
        client.subscribe(channel.clone());
        for frame in h.app.store.replay(&ctx, &channel, None).await?.0 {
            client.apply(frame);
        }
        ensure!(pinned_at(&client, older)? == 0);
        let pin = |conv: Uuid, pinned: bool| {
            let path = format!("/orgs/{org}/conversations/{conv}/settings");
            let (h, user) = (&h, &user);
            async move {
                h.json(
                    user,
                    &path,
                    reqwest::Method::PATCH,
                    json!({ "pinned": pinned }),
                )
                .await
            }
        };
        let catch_up = |cursor: u64| {
            let (h, ctx, channel) = (&h, &ctx, &channel);
            async move {
                let (frames, _) = h.app.store.replay(ctx, channel, Some(cursor)).await?;
                ensure!(frames.len() == 1, "a pin must produce one durable event");
                anyhow::Ok(frames)
            }
        };
        pin(older, true).await?;
        for frame in catch_up(client.channels[&channel].cursor).await? {
            client.apply(frame);
        }
        let first = pinned_at(&client, older)?;
        ensure!(first > 0, "the online client must see the pin");
        // The list stays newest first and carries the pin; clients order pins themselves.
        let list = order().await?;
        ensure!(list.iter().map(|c| id(c, "id")).collect::<Vec<_>>() == [newer, older]);
        ensure!(list[1]["pinned_at_ms"] == first && list[0]["pinned_at_ms"].is_null());
        // A pin outlasts the list's limit.
        sqlx::query(
            "INSERT INTO conversations(id,org_id,creator_id,mode,title,created_at)
             SELECT gen_random_uuid(),$1,$2,'chat','filler',now()+n*interval '1 second'
             FROM generate_series(1,200) n",
        )
        .bind(org)
        .bind(id(&user, "user_id"))
        .execute(&h.db)
        .await?;
        let list = order().await?;
        ensure!(list.len() == 200 && list[199]["id"] == older.to_string());
        sqlx::query("DELETE FROM conversations WHERE org_id=$1 AND title='filler'")
            .bind(org)
            .execute(&h.db)
            .await?;
        // A client starting afresh reads it from the snapshot.
        let mut fresh = ryzhub_core::Engine::default();
        fresh.subscribe(channel.clone());
        for frame in h.app.store.replay(&ctx, &channel, None).await?.0 {
            fresh.apply(frame);
        }
        ensure!(pinned_at(&fresh, older)? == first);
        // Pinning again keeps the original time.
        pin(older, true).await?;
        for frame in catch_up(client.channels[&channel].cursor).await? {
            client.apply(frame);
        }
        ensure!(pinned_at(&client, older)? == first);
        pin(older, false).await?;
        for frame in catch_up(client.channels[&channel].cursor).await? {
            client.apply(frame);
        }
        ensure!(pinned_at(&client, older)? == 0);
        ensure!(order().await?[1]["pinned_at_ms"].is_null());
        let temporary = h
            .configured(&user, "chat", json!({"temporary":true}), None)
            .await?;
        let refused = pin(temporary, true).await;
        ensure!(
            refused
                .as_ref()
                .is_err_and(|e| e.to_string().contains("INVALID_CONVERSATION")),
            "a temporary conversation must not be pinned: {refused:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    h.close().await?;
    outcome
}

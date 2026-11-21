use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::json;
use log::{info, warn, error};

/// Where one file of a message goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadTarget {
    pub id: String,
    pub filename: String,
    pub upload_url: String,
    pub upload_filename: String,
}

/// Discord's answer to an upload request, in the order the files were asked for.
fn parse_upload_targets(body: &serde_json::Value, names: &[String]) -> Result<Vec<UploadTarget>, String> {
    let entries = body["attachments"].as_array().ok_or("Upload answer without attachments")?;
    entries
        .iter()
        .zip(names)
        .map(|(entry, name)| {
            let upload_url = entry["upload_url"].as_str().ok_or("Upload answer without a link")?;
            if !upload_url.starts_with("https://") {
                return Err("Upload link is not HTTPS".to_string());
            }
            Ok(UploadTarget {
                id: entry["id"].as_u64().map(|id| id.to_string())
                    .or_else(|| entry["id"].as_str().map(str::to_owned))
                    .ok_or("Upload answer without an id")?,
                filename: name.clone(),
                upload_url: upload_url.to_owned(),
                upload_filename: entry["upload_filename"].as_str().ok_or("Upload answer without a file name")?.to_owned(),
            })
        })
        .collect()
}

/// Sends a file to its signed link, with the shared client: the storage host never
/// receives the account token.
pub async fn put_upload(url: &str, bytes: Vec<u8>) -> Result<(), String> {
    let response = crate::utils::network_settings::shared_http_client()
        .put(url)
        .timeout(std::time::Duration::from_secs(900))
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("upload failed: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("upload failed (HTTP {})", response.status().as_u16()))
    }
}

#[derive(Clone)]
pub struct DiscordHttpClient {
    client: reqwest::Client,
    #[allow(dead_code)]
    token: String,
}

impl DiscordHttpClient {
    pub fn new(token: String) -> Self {
        let clean_token = token.chars().filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'').collect::<String>();
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&clean_token).unwrap_or(HeaderValue::from_static("")));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Same client properties the Gateway IDENTIFY sends (see utils::client_identity).
        if let Ok(props) = HeaderValue::from_str(&crate::utils::client_identity::super_properties_header()) {
            headers.insert("x-super-properties", props);
        }

        let builder = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(crate::utils::client_identity::user_agent());
        let client = crate::utils::apply_proxy_to_builder(builder)
            .build()
            .unwrap_or_default();

        Self { client, token: clean_token }
    }

    pub async fn get_current_user(&self) -> Result<serde_json::Value, String> {
        let url = "https://discord.com/api/v10/users/@me";
        match self.client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        return Ok(json);
                    }
                    Err("Falha ao ler JSON da resposta".to_string())
                } else if status.as_u16() == 401 {
                    Err("HTTP 401 Unauthorized: Token recusado pelo Discord!".to_string())
                } else {
                    Err(format!("Status HTTP {}", status))
                }
            }
            Err(e) => Err(format!("Erro de rede HTTP: {:?}", e)),
        }
    }

    /// The server list is fetched once at startup and nothing fetches it again, so a
    /// passing Discord error (a 5xx, a dropped connection) used to leave the sidebar
    /// empty for the whole session. Those are retried with a growing pause; any other
    /// status, such as a refused token, is returned at once.
    pub async fn get_user_guilds(&self) -> Result<Vec<serde_json::Value>, String> {
        const RETRY_DELAYS_MS: [u64; 3] = [1_000, 3_000, 8_000];
        let mut attempt = 0;
        loop {
            let (result, transient) = self.get_user_guilds_once().await;
            match result {
                Ok(guilds) => return Ok(guilds),
                Err(e) if transient && attempt < RETRY_DELAYS_MS.len() => {
                    warn!("Lista de servidores falhou ({}), tentando de novo em {} ms", e, RETRY_DELAYS_MS[attempt]);
                    tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MS[attempt])).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn get_user_guilds_once(&self) -> (Result<Vec<serde_json::Value>, String>, bool) {
        let url = "https://discord.com/api/v10/users/@me/guilds";
        match self.client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(guilds) = resp.json::<Vec<serde_json::Value>>().await {
                        return (Ok(guilds), false);
                    }
                }
                let transient = status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                (Err(format!("Status HTTP {}", status)), transient)
            }
            Err(e) => (Err(format!("Erro HTTP: {:?}", e)), true),
        }
    }

    pub async fn get_user_settings(&self) -> Result<serde_json::Value, String> {
        let url = "https://discord.com/api/v10/users/@me/settings";
        match self.client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(settings) = resp.json::<serde_json::Value>().await {
                        return Ok(settings);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP: {:?}", e)),
        }
    }

    /// The account's server folders: its current protobuf settings, or the older JSON ones.
    pub async fn get_guild_folders(&self) -> Vec<crate::ui::guild_folders::Folder> {
        use base64::Engine;
        let url = "https://discord.com/api/v9/users/@me/settings-proto/1";
        if let Ok(resp) = self.client.get(url).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    let decoded = body["settings"]
                        .as_str()
                        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
                    if let Some(settings) = decoded {
                        let folders = crate::ui::guild_folders::parse_proto(&settings);
                        if !folders.is_empty() {
                            return folders;
                        }
                    }
                }
            }
        }
        self.get_user_settings()
            .await
            .map(|settings| crate::ui::guild_folders::parse_json(&settings))
            .unwrap_or_default()
    }

    /// The GIFs the account favorited (its frecency settings).
    pub async fn get_favorite_gifs(&self) -> Result<Vec<crate::ui::gif_picker::Gif>, String> {
        use base64::Engine;
        let url = "https://discord.com/api/v9/users/@me/settings-proto/2";
        let resp = self.client.get(url).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        if !resp.status().is_success() {
            return Err(format!("Status HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("JSON: {:?}", e))?;
        let settings = body["settings"].as_str().unwrap_or("");
        let bytes = base64::engine::general_purpose::STANDARD.decode(settings).map_err(|e| format!("base64: {e}"))?;
        Ok(crate::ui::gif_picker::parse_favorite_gifs(&bytes))
    }

    /// Discord's GIF results: trending for an empty query, a search otherwise.
    pub async fn get_gifs(&self, query: &str) -> Result<serde_json::Value, String> {
        let mut params = vec![("media_format", "gif"), ("provider", "tenor"), ("locale", "en-US"), ("limit", "40")];
        let url = if query.is_empty() {
            "https://discord.com/api/v9/gifs/trending-gifs"
        } else {
            params.push(("q", query));
            "https://discord.com/api/v9/gifs/search"
        };
        let resp = self.client.get(url).query(&params).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        if !resp.status().is_success() {
            return Err(format!("Status HTTP {}", resp.status()));
        }
        resp.json().await.map_err(|e| format!("JSON: {:?}", e))
    }

    pub async fn get_user_dms(&self) -> Result<Vec<serde_json::Value>, String> {
        let url = "https://discord.com/api/v10/users/@me/channels";
        match self.client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(dms) = resp.json::<Vec<serde_json::Value>>().await {
                        return Ok(dms);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP: {:?}", e)),
        }
    }

    pub async fn get_guild_channels(&self, guild_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let url = format!("https://discord.com/api/v10/guilds/{}/channels", guild_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(channels) = resp.json::<Vec<serde_json::Value>>().await {
                        return Ok(channels);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP: {:?}", e)),
        }
    }

    pub async fn get_guild_onboarding(&self, guild_id: &str) -> Result<serde_json::Value, String> {
        let url = format!("https://discord.com/api/v10/guilds/{}/onboarding", guild_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        return Ok(json);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP: {:?}", e)),
        }
    }

    /// Saves the answers of the community onboarding questionnaire.
    ///
    /// A member that never finished the flow answers with POST. Once Discord has
    /// marked onboarding as completed (for example after a previous client), the
    /// same endpoint only accepts edits through PUT and answers 403 to POST.
    pub async fn submit_guild_onboarding(
        &self,
        guild_id: &str,
        body: &serde_json::Value,
    ) -> Result<(), String> {
        let url = format!("https://discord.com/api/v10/guilds/{}/onboarding-responses", guild_id);
        let response = self.client.post(&url).json(body).send().await
            .map_err(|e| format!("Falha de rede ao enviar onboarding: {:?}", e))?;
        let status = response.status();
        if status.is_success() {
            info!("Onboarding do servidor {} enviado.", guild_id);
            return Ok(());
        }
        let first_error = response.text().await.unwrap_or_default();
        if status.as_u16() != 403 && status.as_u16() != 405 {
            let err = format!("Onboarding HTTP {}: {}", status, first_error);
            error!("{}", err);
            return Err(err);
        }

        let response = self.client.put(&url).json(body).send().await
            .map_err(|e| format!("Falha de rede ao enviar onboarding: {:?}", e))?;
        let status = response.status();
        if status.is_success() {
            info!("Onboarding do servidor {} atualizado.", guild_id);
            return Ok(());
        }
        let err = format!("Onboarding HTTP {}: {}", status, response.text().await.unwrap_or_default());
        error!("{}", err);
        Err(err)
    }

    pub async fn get_channel_permissions(&self, guild_id: &str) -> Result<crate::channel_permissions::ChannelPermissions, String> {
        if let Some(permissions) = crate::channel_permissions::cached(guild_id) {
            return Ok(permissions);
        }
        // The gateway already said who the user is; asking again cost a round trip.
        let user_id = match crate::gateway::get_my_user_id() {
            0 => {
                let user = self.get_current_user().await?;
                user["id"].as_str().ok_or("Missing current user id")?.to_string()
            }
            id => id.to_string(),
        };
        let guild_url = format!("https://discord.com/api/v10/guilds/{}", guild_id);
        let member_url = format!("https://discord.com/api/v10/guilds/{}/members/{}", guild_id, user_id);
        let (guild, member) = tokio::try_join!(
            self.get_permission_data(&guild_url), self.get_permission_data(&member_url)
        )?;
        let permissions = crate::channel_permissions::ChannelPermissions::new(&guild, &member, &user_id)?;
        crate::channel_permissions::remember(&permissions);
        Ok(permissions)
    }

    async fn get_permission_data(&self, url: &str) -> Result<serde_json::Value, String> {
        let response = self.client.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("Permission lookup HTTP {}", response.status()));
        }
        response.json().await.map_err(|e| e.to_string())
    }

    pub async fn get_channel_messages(&self, channel_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let url = format!("https://discord.com/api/v10/channels/{}/messages?limit=30", channel_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(msgs) = resp.json::<Vec<serde_json::Value>>().await {
                        return Ok(msgs);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP: {:?}", e)),
        }
    }

    pub async fn get_channel_messages_before(&self, channel_id: &str, before_msg_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let url = format!("https://discord.com/api/v10/channels/{}/messages?limit=30&before={}", channel_id, before_msg_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(msgs) = resp.json::<Vec<serde_json::Value>>().await {
                        return Ok(msgs);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP: {:?}", e)),
        }
    }

    pub async fn download_image(&self, url: &str) -> Result<Vec<u8>, String> {
        match self.client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(bytes) = resp.bytes().await {
                        return Ok(bytes.to_vec());
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP: {:?}", e)),
        }
    }

    pub async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), String> {
        let url = format!("https://discord.com/api/v10/channels/{}/messages", channel_id);
        let payload = json!({ "content": content });

        match self.client.post(&url).json(&payload).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!("Mensagem enviada com sucesso para o canal {}", channel_id);
                    Ok(())
                } else {
                    let err = format!("Status de erro HTTP: {}", resp.status());
                    error!("{}", err);
                    Err(err)
                }
            }
            Err(e) => {
                let err = format!("Falha ao enviar mensagem via HTTP: {:?}", e);
                error!("{}", err);
                Err(err)
            }
        }
    }

    #[allow(dead_code)]
    pub async fn update_my_voice_state(&self, guild_id: &str, channel_id: &str) -> Result<(), String> {
        let url = format!("https://discord.com/api/v10/guilds/{}/voice-states/@me", guild_id);
        let payload = json!({
            "channel_id": channel_id,
            "suppress": false
        });

        match self.client.patch(&url).json(&payload).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!("Voice State REST @me atualizado com SUCESSO no servidor {}!", guild_id);
                    Ok(())
                } else {
                    let err = format!("Status de erro HTTP Voice State: {}", resp.status());
                    error!("{}", err);
                    Err(err)
                }
            }
            Err(e) => {
                let err = format!("Falha ao atualizar Voice State via HTTP: {:?}", e);
                error!("{}", err);
                Err(err)
            }
        }
    }

    /// A scheduled event of a server the user is in, with its interested count.
    pub async fn get_scheduled_event(&self, guild_id: &str, event_id: &str) -> Result<serde_json::Value, String> {
        let url = format!("https://discord.com/api/v10/guilds/{guild_id}/scheduled-events/{event_id}?with_user_count=true");
        let resp = self.client.get(&url).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Status HTTP {}", status));
        }
        resp.json::<serde_json::Value>().await.map_err(|e| format!("JSON: {:?}", e))
    }

    pub async fn get_user_profile(&self, user_id: &str) -> Result<serde_json::Value, String> {
        let url = format!("https://discord.com/api/v10/users/{}", user_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(user) = resp.json::<serde_json::Value>().await {
                        return Ok(user);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP ao buscar usuário: {:?}", e)),
        }
    }

    pub async fn get_guild_member(&self, guild_id: &str, user_id: &str) -> Result<serde_json::Value, String> {
        let url = format!("https://discord.com/api/v10/guilds/{}/members/{}", guild_id, user_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(member) = resp.json::<serde_json::Value>().await {
                        return Ok(member);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP ao buscar membro do servidor: {:?}", e)),
        }
    }

    /// A user's full profile, with their profile in this server when one is given.
    pub async fn get_full_profile(&self, user_id: &str, guild_id: Option<&str>) -> Result<serde_json::Value, String> {
        let mut url = format!("https://discord.com/api/v9/users/{}/profile?with_mutual_guilds=false&with_mutual_friends=false", user_id);
        if let Some(guild_id) = guild_id {
            url.push_str(&format!("&guild_id={}", guild_id));
        }
        if let Ok(resp) = self.client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(profile) = resp.json::<serde_json::Value>().await {
                    return Ok(profile);
                }
            }
        }
        self.get_user_profile(user_id).await
    }

    /// Asks Discord for one upload link per file; refuses here if a file is over the limit.
    pub async fn request_upload_urls(&self, channel_id: &str, files: &[(String, u64)]) -> Result<Vec<UploadTarget>, String> {
        let url = format!("https://discord.com/api/v9/channels/{}/attachments", channel_id);
        let list: Vec<serde_json::Value> = files
            .iter()
            .enumerate()
            .map(|(index, (name, size))| json!({ "id": index.to_string(), "filename": name, "file_size": size }))
            .collect();
        let resp = self.client.post(&url).json(&json!({ "files": list })).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if status.as_u16() == 413 || body["code"].as_u64() == Some(40005) {
            return Err("File is too large for this server or your plan".to_string());
        }
        if status.as_u16() == 403 {
            return Err("You can't send files in this channel".to_string());
        }
        if !status.is_success() {
            return Err(format!("Discord error {}", status.as_u16()));
        }
        let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
        parse_upload_targets(&body, &names)
    }

    pub async fn send_message_with_files(&self, channel_id: &str, content: &str, files: &[UploadTarget]) -> Result<(), String> {
        let url = format!("https://discord.com/api/v10/channels/{}/messages", channel_id);
        let attachments: Vec<serde_json::Value> = files
            .iter()
            .map(|file| json!({ "id": file.id, "filename": file.filename, "uploaded_filename": file.upload_filename }))
            .collect();
        let payload = json!({ "content": content, "attachments": attachments });
        let resp = self.client.post(&url).json(&payload).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("Discord error {}", resp.status().as_u16()))
        }
    }

    /// Opens (or finds) the DM with a user and returns its channel id.
    pub async fn open_dm(&self, user_id: &str) -> Result<String, String> {
        let url = "https://discord.com/api/v9/users/@me/channels";
        let resp = self.client.post(url).json(&json!({ "recipients": [user_id] })).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        if !resp.status().is_success() {
            return Err(format!("Status HTTP {}", resp.status()));
        }
        let channel: serde_json::Value = resp.json().await.map_err(|e| format!("JSON: {:?}", e))?;
        channel["id"].as_str().map(str::to_owned).ok_or_else(|| "Missing channel id".to_string())
    }

    async fn moderation_result(result: Result<reqwest::Response, reqwest::Error>) -> Result<(), String> {
        let response = result.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            403 => Err("Discord refused: missing permission or role too low".to_string()),
            code => Err(format!("Discord error {code}")),
        }
    }

    /// Server mute, server deafen, or disconnecting from voice (`channel_id: null`).
    pub async fn update_member(&self, guild_id: &str, user_id: &str, change: serde_json::Value) -> Result<(), String> {
        let url = format!("https://discord.com/api/v9/guilds/{}/members/{}", guild_id, user_id);
        Self::moderation_result(self.client.patch(&url).json(&change).send().await).await
    }

    pub async fn kick_member(&self, guild_id: &str, user_id: &str) -> Result<(), String> {
        let url = format!("https://discord.com/api/v9/guilds/{}/members/{}", guild_id, user_id);
        Self::moderation_result(self.client.delete(&url).send().await).await
    }

    pub async fn ban_member(&self, guild_id: &str, user_id: &str) -> Result<(), String> {
        let url = format!("https://discord.com/api/v9/guilds/{}/bans/{}", guild_id, user_id);
        Self::moderation_result(self.client.put(&url).json(&json!({})).send().await).await
    }

    /// The server's roles, for member colors and role groups.
    pub async fn get_guild_roles(&self, guild_id: &str) -> Result<Vec<crate::ui::member_list::Role>, String> {
        let url = format!("https://discord.com/api/v10/guilds/{}", guild_id);
        let guild = self.get_permission_data(&url).await?;
        Ok(guild["roles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|role| {
                Some(crate::ui::member_list::Role {
                    id: role["id"].as_str()?.to_owned(),
                    name: role["name"].as_str().unwrap_or("").to_owned(),
                    color: role["color"].as_u64().unwrap_or(0) as u32,
                    position: role["position"].as_i64().unwrap_or(0),
                })
            })
            .collect())
    }

    pub async fn get_guild_members(&self, guild_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let url = format!("https://discord.com/api/v10/guilds/{}/members?limit=1000", guild_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(members) = resp.json::<Vec<serde_json::Value>>().await {
                        return Ok(members);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP ao buscar membros do servidor: {:?}", e)),
        }
    }

    pub async fn get_guild_application_command_index(&self, guild_id: &str) -> Result<serde_json::Value, String> {
        let url = format!("https://discord.com/api/v9/guilds/{}/application-command-index", guild_id);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    if let Ok(data) = resp.json::<serde_json::Value>().await {
                        return Ok(data);
                    }
                }
                Err(format!("Status HTTP {}", status))
            }
            Err(e) => Err(format!("Erro HTTP ao buscar índice de comandos do servidor: {:?}", e)),
        }
    }

    pub async fn send_slash_command_interaction(
        &self,
        guild_id: &str,
        channel_id: &str,
        app_id: &str,
        cmd_id: &str,
        cmd_name: &str,
        version: &str,
        options: Vec<serde_json::Value>,
        session_id: &str,
    ) -> Result<(), String> {
        let url = "https://discord.com/api/v9/interactions";
        let nonce = format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis());
        
        let payload = json!({
            "type": 2,
            "application_id": app_id,
            "guild_id": guild_id,
            "channel_id": channel_id,
            "session_id": if session_id.is_empty() { "a0000000000000000000000000000000" } else { session_id },
            "data": {
                "version": if version.is_empty() { cmd_id } else { version },
                "id": cmd_id,
                "name": cmd_name,
                "type": 1,
                "options": options
            },
            "nonce": nonce
        });

        match self.client.post(url).json(&payload).send().await {
            Ok(resp) => {
                if resp.status().is_success() || resp.status().as_u16() == 204 {
                    info!("Slash command interaction '/{}' enviada com sucesso!", cmd_name);
                    Ok(())
                } else {
                    let err = format!("Status de erro HTTP na interação: {}", resp.status());
                    error!("{}", err);
                    Err(err)
                }
            }
            Err(e) => {
                let err = format!("Falha de rede ao enviar slash command interaction: {:?}", e);
                error!("{}", err);
                Err(err)
            }
        }
    }

    pub async fn ring_dm_call(&self, channel_id: &str) -> Result<(), String> {
        let url = format!("https://discord.com/api/v9/channels/{}/call/ring", channel_id);
        let payload = json!({ "recipients": serde_json::Value::Null });

        match self.client.post(&url).json(&payload).send().await {
            Ok(resp) => {
                if resp.status().is_success() || resp.status().as_u16() == 204 {
                    info!("🔔 Chamada de DM tocando no canal {}...", channel_id);
                    Ok(())
                } else {
                    let err = format!("Status de erro HTTP ao iniciar toque da DM: {}", resp.status());
                    warn!("{}", err);
                    Err(err)
                }
            }
            Err(e) => {
                let err = format!("Falha de rede ao iniciar toque da DM: {:?}", e);
                warn!("{}", err);
                Err(err)
            }
        }
    }

    /// Hides a one-to-one DM. The channel's type is checked first, because the same
    /// request on a group DM would leave the group.
    pub async fn close_dm(&self, channel_id: &str) -> Result<(), String> {
        let url = format!("https://discord.com/api/v9/channels/{}", channel_id);
        let response = self.client.get(&url).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        if !response.status().is_success() {
            return Err(format!("Status HTTP {}", response.status()));
        }
        let channel: serde_json::Value = response.json().await.map_err(|e| format!("JSON: {:?}", e))?;
        if !crate::ui::dm_close::is_closable(channel["type"].as_u64().unwrap_or(u64::MAX)) {
            return Err("Only one-to-one DMs can be closed".to_string());
        }
        let response = self.client.delete(&url).send().await.map_err(|e| format!("Erro HTTP: {:?}", e))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("Status HTTP {}", response.status()))
        }
    }

    pub async fn stop_ringing_dm_call(&self, channel_id: &str) -> Result<(), String> {
        let url = format!("https://discord.com/api/v9/channels/{}/call/stop-ringing", channel_id);
        let payload = json!({ "recipients": serde_json::Value::Null });

        match self.client.post(&url).json(&payload).send().await {
            Ok(resp) => {
                if resp.status().is_success() || resp.status().as_u16() == 204 {
                    info!("🔕 Toque da chamada de DM interrompido no canal {}.", channel_id);
                    Ok(())
                } else {
                    let err = format!("Status de erro HTTP ao parar toque da DM: {}", resp.status());
                    warn!("{}", err);
                    Err(err)
                }
            }
            Err(e) => {
                let err = format!("Falha de rede ao parar toque da DM: {:?}", e);
                warn!("{}", err);
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    #[test]
    fn upload_targets_keep_the_order_and_names_asked_for() {
        let body = json!({ "attachments": [
            { "id": 0, "upload_url": "https://storage/a", "upload_filename": "tmp/a" },
            { "id": "1", "upload_url": "https://storage/b", "upload_filename": "tmp/b" }
        ]});
        let targets = parse_upload_targets(&body, &["a.png".to_string(), "b.txt".to_string()]).unwrap();
        assert_eq!(targets[0].id, "0");
        assert_eq!(targets[1].filename, "b.txt");
        assert_eq!(targets[1].upload_filename, "tmp/b");
    }

    #[test]
    fn an_upload_link_that_is_not_https_is_refused() {
        let body = json!({ "attachments": [{ "id": 0, "upload_url": "http://storage/a", "upload_filename": "x" }] });
        assert!(parse_upload_targets(&body, &["a".to_string()]).is_err());
    }
}

use std::{path, vec};

use anyhow::{Context, Result};

use crate::{
    deltas, gb_repository, project_repository, projects,
    reader::{self, Reader},
    sessions, users,
};

use super::events;

#[derive(Clone)]
pub struct Handler {
    project_id: String,
    project_store: projects::Storage,
    local_data_dir: path::PathBuf,
    user_store: users::Storage,
}

impl Handler {
    pub fn new(
        local_data_dir: path::PathBuf,
        project_id: String,
        project_store: projects::Storage,
        user_store: users::Storage,
    ) -> Self {
        Self {
            project_id,
            project_store,
            local_data_dir,
            user_store,
        }
    }

    // Returns Some(file_content) or None if the file is ignored.
    fn get_current_file_content(
        &self,
        project_repository: &project_repository::Repository,
        path: &std::path::Path,
    ) -> Result<Option<String>> {
        if project_repository.is_path_ignored(path)? {
            return Ok(None);
        }

        let reader = project_repository.get_wd_reader();

        let path = path.to_str().unwrap();
        if !reader.exists(path) {
            return Ok(Some(String::new()));
        }

        if reader.size(path)? > 100_000 {
            log::warn!("{}: ignoring large file: {}", self.project_id, path);
            return Ok(None);
        }

        match reader.read(path)? {
            reader::Content::UTF8(content) => Ok(Some(content)),
            reader::Content::Binary(_) => {
                log::warn!("{}: ignoring non-utf8 file: {}", self.project_id, path);
                Ok(None)
            }
        }
    }

    // returns deltas for the file that are already part of the current session (if any)
    fn get_current_deltas(&self, path: &std::path::Path) -> Result<Option<Vec<deltas::Delta>>> {
        let gb_repo = gb_repository::Repository::open(
            self.local_data_dir.clone(),
            self.project_id.clone(),
            self.project_store.clone(),
            self.user_store.clone(),
        )
        .context("failed to open gb repository")?;

        let current_session = gb_repo.get_current_session()?;
        if current_session.is_none() {
            return Ok(None);
        }
        let current_session = current_session.unwrap();
        let session_reader = sessions::Reader::open(&gb_repo, &current_session)
            .context("failed to get session reader")?;
        let deltas_reader = deltas::Reader::new(&session_reader);
        let deltas = deltas_reader
            .read_file(path)
            .context("failed to get file deltas")?;
        Ok(deltas)
    }

    pub fn handle<P: AsRef<std::path::Path>>(&self, path: P) -> Result<Vec<events::Event>> {
        let project = self
            .project_store
            .get_project(&self.project_id)
            .context("failed to get project")?
            .ok_or_else(|| anyhow::anyhow!("project not found"))?;

        let project_repository = project_repository::Repository::open(&project)
            .with_context(|| "failed to open project repository for project")?;

        let gb_repository = gb_repository::Repository::open(
            &self.local_data_dir,
            self.project_id.clone(),
            self.project_store.clone(),
            self.user_store.clone(),
        )
        .context("failed to open gb repository")?;

        // If current session's branch is not the same as the project's head, flush it first.
        if let Some(session) = gb_repository
            .get_current_session()
            .context("failed to get current session")?
        {
            let project_head = project_repository
                .get_head()
                .context("failed to get head")?;
            if session.meta.branch != project_head.name().map(|s| s.to_string()) {
                gb_repository
                    .flush_session(&project_repository, &session)
                    .context("failed to flush session")?;
            }
        }

        let path = path.as_ref();

        let current_wd_file_content = match self
            .get_current_file_content(&project_repository, path)
            .context("failed to get current file content")?
        {
            Some(content) => content,
            None => return Ok(vec![]),
        };

        let current_session = gb_repository
            .get_or_create_current_session()
            .context("failed to get or create current session")?;
        let current_session_reader = sessions::Reader::open(&gb_repository, &current_session)
            .context("failed to get session reader")?;

        let latest_file_content = match current_session_reader.file(path) {
            Ok(reader::Content::UTF8(content)) => content,
            Ok(reader::Content::Binary(_)) => {
                log::warn!(
                    "{}: ignoring non-utf8 file: {}",
                    self.project_id,
                    path.display()
                );
                return Ok(vec![]);
            }
            Err(reader::Error::NotFound) => "".to_string(),
            Err(err) => Err(err).context("failed to get file content")?,
        };

        let current_deltas = self
            .get_current_deltas(path)
            .with_context(|| "failed to get current deltas")?;

        let mut text_doc = deltas::Document::new(
            Some(&latest_file_content),
            current_deltas.unwrap_or_default(),
        )?;

        let new_delta = text_doc
            .update(&current_wd_file_content)
            .context("failed to calculate new deltas")?;
        if new_delta.is_none() {
            log::debug!(
                "{}: {} no new deltas, ignoring",
                self.project_id,
                path.display()
            );
            return Ok(vec![]);
        }
        let new_delta = new_delta.as_ref().unwrap();

        let deltas = text_doc.get_deltas();
        let writer = deltas::Writer::new(&gb_repository);
        writer
            .write(path, &deltas)
            .with_context(|| "failed to write deltas")?;
        writer
            .write_wd_file(path, &current_wd_file_content)
            .with_context(|| "failed to write file")?;

        Ok(vec![
            events::Event::SessionFile((
                current_session.id.clone(),
                path.to_path_buf(),
                latest_file_content,
            )),
            events::Event::Session(current_session.clone()),
            events::Event::SessionDelta((
                current_session.id.clone(),
                path.to_path_buf(),
                new_delta.clone(),
            )),
        ])
    }
}

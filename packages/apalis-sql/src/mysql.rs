use apalis_core::error::{JobError, JobStreamError};
use apalis_core::job::{Counts, Job, JobStreamExt, JobStreamResult, JobStreamWorker};
use apalis_core::request::{JobRequest, JobState};
use apalis_core::storage::StorageError;
use apalis_core::storage::StorageWorkerPulse;
use apalis_core::storage::{Storage, StorageResult};
use async_stream::try_stream;
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::{de::DeserializeOwned, Serialize};

use sqlx::types::Uuid;
use sqlx::{MySql, MySqlPool, Pool, Row};
use std::collections::HashMap;
use std::convert::TryInto;
use std::ops::Sub;
use std::{marker::PhantomData, ops::Add, time::Duration};

use crate::from_row::{IntoJobRequest, SqlJobRequest};

/// Represents a [Storage] that persists to MySQL
#[derive(Debug)]
pub struct MysqlStorage<T> {
    pool: Pool<MySql>,
    job_type: PhantomData<T>,
}

impl<T> Clone for MysqlStorage<T> {
    fn clone(&self) -> Self {
        let pool = self.pool.clone();
        MysqlStorage {
            pool,
            job_type: PhantomData,
        }
    }
}

impl<T> MysqlStorage<T> {
    /// Create a new instance from a pool
    pub fn new(pool: MySqlPool) -> Self {
        Self {
            pool,
            job_type: PhantomData,
        }
    }
    /// Create a Mysql Connection and start a Storage
    pub async fn connect<S: Into<String>>(db: S) -> Result<Self, sqlx::Error> {
        let pool = MySqlPool::connect(&db.into()).await?;
        Ok(Self::new(pool))
    }

    /// Setup
    #[cfg(feature = "migrate")]
    pub async fn setup(&self) -> Result<(), sqlx::Error> {
        let pool = self.pool.clone();
        sqlx::migrate!("migrations/mysql").run(&pool).await?;
        Ok(())
    }
}

impl<T: DeserializeOwned + Send + Unpin + Job> MysqlStorage<T> {
    fn stream_jobs(
        &self,
        worker_id: String,
        interval: Duration,
    ) -> impl Stream<Item = Result<Option<JobRequest<T>>, JobStreamError>> {
        let pool = self.pool.clone();
        let mut interval = tokio::time::interval(interval);
        try_stream! {
            loop {
                interval.tick().await;
                let pool = pool.clone();
                let mut tx = pool
                .begin()
                .await
                .map_err(|e| JobStreamError::BrokenPipe(Box::from(e)))?;

                let job_type = T::NAME;
                let fetch_query = "SELECT * FROM jobs
                    WHERE status = 'Pending' AND run_at <= NOW() AND job_type = ? ORDER BY run_at ASC LIMIT 1 FOR UPDATE";
                let job: Option<SqlJobRequest<T>> = sqlx::query_as(fetch_query)
                    .bind(job_type)
                    .fetch_optional(&mut tx)
                    .await.map_err(|e| JobStreamError::BrokenPipe(Box::from(e)))?;
                yield match job.build_job_request() {
                        None => {
                            tx.commit()
                                .await
                                .map_err(|e| JobStreamError::BrokenPipe(Box::from(e)))?;
                                None
                        },
                        Some(job) => {

                            let job_id = job.id();
                            let update_query = "UPDATE jobs SET status = 'Running', lock_by = ?, lock_at = NOW() WHERE id = ? AND status = 'Pending' AND lock_by IS NULL;";
                            sqlx::query(update_query)
                                .bind(worker_id.clone())
                                .bind(job_id.clone())
                                .execute(&mut tx)
                                .await
                                .map_err(|e| JobStreamError::BrokenPipe(Box::from(e)))?;
                            let job: Option<SqlJobRequest<T>> = sqlx::query_as("Select * from jobs where id = ?")
                                .bind(job_id.clone())
                                .fetch_optional(&mut tx)
                                .await
                                .map_err(|e| JobStreamError::BrokenPipe(Box::from(e)))?;

                            tx.commit()
                                .await
                                .map_err(|e| JobStreamError::BrokenPipe(Box::from(e)))?;

                            job.build_job_request()
                        }
                    }
                    // yield fetch_next(pool.clone(), worker_id.clone(), job.as_job_request()).await.map_err(|e| JobStreamError::BrokenPipe(Box::from(e)))?
            }
        }
    }

    async fn keep_alive_at<Service>(
        &mut self,
        worker_id: String,
        last_seen: DateTime<Utc>,
    ) -> StorageResult<()> {
        let pool = self.pool.clone();

        let mut tx = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Connection(Box::from(e)))?;
        let worker_type = T::NAME;
        let storage_name = std::any::type_name::<Self>();
        let query =
            "REPLACE INTO workers (id, worker_type, storage_name, layers, last_seen) VALUES (?, ?, ?, ?, ?);";
        sqlx::query(query)
            .bind(worker_id.to_owned())
            .bind(worker_type)
            .bind(storage_name)
            .bind(std::any::type_name::<Service>())
            .bind(last_seen)
            .execute(&mut tx)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl<T> Storage for MysqlStorage<T>
where
    T: Job + Serialize + DeserializeOwned + Send + 'static + Unpin,
{
    type Output = T;

    async fn push(&mut self, job: Self::Output) -> StorageResult<()> {
        let id = Uuid::new_v4();
        let query =
            "INSERT INTO jobs VALUES (?, ?, ?, 'Pending', 0, 25, now(), NULL, NULL, NULL, NULL)";
        let pool = self.pool.clone();

        let job = serde_json::to_string(&job)?;
        let mut pool = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Connection(Box::from(e)))?;
        let job_type = T::NAME;
        sqlx::query(query)
            .bind(job)
            .bind(id.to_string())
            .bind(job_type.to_string())
            .execute(&mut pool)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }

    async fn schedule(
        &mut self,
        job: Self::Output,
        on: chrono::DateTime<Utc>,
    ) -> StorageResult<()> {
        let query =
            "INSERT INTO jobs VALUES (?, ?, ?, 'Pending', 0, 25, ?, NULL, NULL, NULL, NULL)";
        let pool = self.pool.clone();
        let id = Uuid::new_v4();

        let job = serde_json::to_string(&job)?;
        let mut pool = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Connection(Box::from(e)))?;
        let job_type = T::NAME;
        sqlx::query(query)
            .bind(job)
            .bind(id.to_string())
            .bind(job_type)
            .bind(on)
            .execute(&mut pool)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }

    async fn fetch_by_id(&self, job_id: String) -> StorageResult<Option<JobRequest<Self::Output>>> {
        let pool = self.pool.clone();

        let mut conn = pool
            .clone()
            .acquire()
            .await
            .map_err(|e| StorageError::Connection(Box::from(e)))?;
        let fetch_query = "SELECT * FROM jobs WHERE id = ?";
        let res: Option<SqlJobRequest<T>> = sqlx::query_as(fetch_query)
            .bind(job_id)
            .fetch_optional(&mut conn)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(res.build_job_request())
    }

    async fn heartbeat(&mut self, pulse: StorageWorkerPulse) -> StorageResult<bool> {
        let pool = self.pool.clone();

        match pulse {
            StorageWorkerPulse::EnqueueScheduled { count: _ } => {
                // Idealy jobs are queue via run_at. So this is not necessary
                Ok(true)
            }
            // Worker not seen in 5 minutes yet has running jobs
            StorageWorkerPulse::RenqueueOrpharned { count } => {
                let job_type = T::NAME;
                let mut tx = pool
                    .acquire()
                    .await
                    .map_err(|e| StorageError::Database(Box::from(e)))?;
                let query = r#"Update jobs 
                        INNER JOIN workers ON jobs.lock_by = workers.id
                        SET status = "Pending", done_at = NULL, lock_by = NULL, lock_at = NULL, last_error ="Job was abandoned"
                        WHERE status = "Running" AND workers.last_seen < ? AND workers.worker_type = ?
                        ORDER BY lock_at ASC LIMIT ?;"#;
                sqlx::query(query)
                    .bind(Utc::now().sub(chrono::Duration::minutes(5)))
                    .bind(job_type)
                    .bind(count)
                    .execute(&mut tx)
                    .await
                    .map_err(|e| StorageError::Database(Box::from(e)))?;
                Ok(true)
            }
            _ => todo!(),
        }
    }

    async fn kill(&mut self, worker_id: String, job_id: String) -> StorageResult<()> {
        let pool = self.pool.clone();

        let mut tx = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Connection(Box::from(e)))?;
        let query =
            "UPDATE jobs SET status = 'Killed', done_at = NOW() WHERE id = ? AND lock_by = ?";
        sqlx::query(query)
            .bind(job_id.to_owned())
            .bind(worker_id.to_owned())
            .execute(&mut tx)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }

    /// Puts the job instantly back into the queue
    /// Another [Worker] may consume
    async fn retry(&mut self, worker_id: String, job_id: String) -> StorageResult<()> {
        let pool = self.pool.clone();

        let mut tx = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        let query =
                "UPDATE jobs SET status = 'Pending', done_at = NULL, lock_by = NULL WHERE id = ? AND lock_by = ?";
        sqlx::query(query)
            .bind(job_id.to_owned())
            .bind(worker_id.to_owned())
            .execute(&mut tx)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }

    fn consume(&mut self, worker_id: String, interval: Duration) -> JobStreamResult<T> {
        Box::pin(self.stream_jobs(worker_id, interval))
    }
    async fn len(&self) -> StorageResult<i64> {
        let pool = self.pool.clone();

        let mut tx = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        let query = "Select Count(*) as count from jobs where status='Pending'";
        let record = sqlx::query(query)
            .fetch_one(&mut tx)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(record
            .try_get("count")
            .map_err(|e| StorageError::Database(Box::from(e)))?)
    }
    async fn ack(&mut self, worker_id: String, job_id: String) -> StorageResult<()> {
        let pool = self.pool.clone();

        let mut tx = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        let query = "UPDATE jobs SET status = 'Done', done_at = now() WHERE id = ? AND lock_by = ?";
        sqlx::query(query)
            .bind(job_id.to_owned())
            .bind(worker_id.to_owned())
            .execute(&mut tx)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }

    async fn reschedule(&mut self, job: &JobRequest<T>, wait: Duration) -> StorageResult<()> {
        let pool = self.pool.clone();
        let job_id = job.id();

        let wait: i64 = wait
            .as_secs()
            .try_into()
            .map_err(|e| StorageError::Database(Box::new(e)))?;
        let wait = chrono::Duration::seconds(wait);
        let mut tx = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Connection(Box::from(e)))?;
        let query =
                "UPDATE jobs SET status = 'Pending', done_at = NULL, lock_by = NULL, lock_at = NULL, run_at = ? WHERE id = ?";
        sqlx::query(query)
            .bind(Utc::now().add(wait))
            .bind(job_id.to_owned())
            .execute(&mut tx)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }

    async fn update_by_id(
        &self,
        job_id: String,
        job: &JobRequest<Self::Output>,
    ) -> StorageResult<()> {
        let pool = self.pool.clone();
        let status = job.status().as_ref().to_string();
        let attempts = job.attempts();
        let done_at = *job.done_at();
        let lock_by = job.lock_by().clone();
        let lock_at = *job.lock_at();
        let last_error = job.last_error().clone();

        let mut tx = pool
            .acquire()
            .await
            .map_err(|e| StorageError::Connection(Box::from(e)))?;
        let query =
                "UPDATE jobs SET status = ?, attempts = ?, done_at = ?, lock_by = ?, lock_at = ?, last_error = ? WHERE id = ?";
        sqlx::query(query)
            .bind(status.to_owned())
            .bind(attempts)
            .bind(done_at)
            .bind(lock_by)
            .bind(lock_at)
            .bind(last_error)
            .bind(job_id.to_owned())
            .execute(&mut tx)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(())
    }

    async fn keep_alive<Service>(&mut self, worker_id: String) -> StorageResult<()> {
        self.keep_alive_at::<Service>(worker_id, Utc::now()).await
    }
}

#[async_trait::async_trait]

impl<J: 'static + Job + Serialize + DeserializeOwned> JobStreamExt<J> for MysqlStorage<J> {
    async fn counts(&mut self) -> Result<Counts, JobError> {
        let mut conn = self
            .pool
            .clone()
            .acquire()
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;

        let fetch_query = "SELECT
            COUNT(CASE WHEN status = 'Pending' THEN 1 END) AS pending,
            COUNT(CASE WHEN status = 'Running' THEN 1 END) AS running,
            COUNT(CASE WHEN status = 'Done' THEN 1 END) AS done,
            COUNT(CASE WHEN status = 'Retry' THEN 1 END) AS retry,
            COUNT(CASE WHEN status = 'Failed' THEN 1 END) AS failed,
            COUNT(CASE WHEN status = 'Killed' THEN 1 END) AS killed
        FROM jobs WHERE job_type = ?";
        let res: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(fetch_query)
            .bind(J::NAME)
            .fetch_one(&mut conn)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        let mut inner = HashMap::new();
        inner.insert(JobState::Pending, res.0);
        inner.insert(JobState::Running, res.1);
        inner.insert(JobState::Done, res.2);
        inner.insert(JobState::Retry, res.3);
        inner.insert(JobState::Failed, res.4);
        inner.insert(JobState::Killed, res.5);
        Ok(Counts { inner })
    }

    async fn list_jobs(
        &mut self,
        status: &JobState,
        page: i32,
    ) -> Result<Vec<JobRequest<J>>, JobError> {
        let status = status.as_ref().to_string();

        let mut conn = self
            .pool
            .clone()
            .acquire()
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        let fetch_query = "SELECT * FROM jobs WHERE status = ? AND job_type = ? ORDER BY done_at DESC, run_at DESC LIMIT 10 OFFSET ?";
        let res: Vec<SqlJobRequest<J>> = sqlx::query_as(fetch_query)
            .bind(status)
            .bind(J::NAME)
            .bind((page - 1) * 10)
            .fetch_all(&mut conn)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(res.into_iter().map(|j| j.into()).collect())
    }

    async fn list_workers(&mut self) -> Result<Vec<JobStreamWorker>, JobError> {
        let mut conn = self
            .pool
            .clone()
            .acquire()
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        let fetch_query =
            "SELECT id, layers, last_seen FROM workers WHERE worker_type = ? ORDER BY last_seen DESC LIMIT 20 OFFSET ?";
        let res: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as(fetch_query)
            .bind(J::NAME)
            .bind(0)
            .fetch_all(&mut conn)
            .await
            .map_err(|e| StorageError::Database(Box::from(e)))?;
        Ok(res
            .into_iter()
            .map(|(worker_id, layers, last_seen)| {
                let mut worker = JobStreamWorker::new::<Self, J>(worker_id, last_seen);
                worker.set_layers(layers);
                worker
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use email_service::Email;
    use futures::StreamExt;

    /// migrate DB and return a storage instance.
    async fn setup() -> MysqlStorage<Email> {
        let db_url = &std::env::var("DATABASE_URL").expect("No DATABASE_URL is specified");
        // Because connections cannot be shared across async runtime
        // (different runtimes are created for each test),
        // we don't share the storage and tests must be run sequentially.
        let storage = MysqlStorage::connect(db_url)
            .await
            .expect("DATABASE_URL is wrong");
        storage.setup().await.expect("failed to migrate DB");
        storage
    }

    /// rollback DB changes made by tests.
    /// Delete the following rows:
    ///  - jobs whose state is `Pending` or locked by `worker_id`
    ///  - worker identified by `worker_id`
    ///
    /// You should execute this function in the end of a test
    async fn cleanup(storage: MysqlStorage<Email>, worker_id: String) {
        sqlx::query("DELETE FROM jobs WHERE lock_by = ? OR status = 'Pending'")
            .bind(worker_id.clone())
            .execute(&storage.pool)
            .await
            .expect("failed to delete jobs");
        sqlx::query("DELETE FROM workers WHERE id = ?")
            .bind(worker_id.clone())
            .execute(&storage.pool)
            .await
            .expect("failed to delete worker");
    }

    async fn consume_one<S, T>(storage: &mut S, worker_id: &String) -> JobRequest<T>
    where
        S: Storage<Output = T>,
    {
        let mut stream = storage.consume(worker_id.clone(), std::time::Duration::from_secs(10));
        stream
            .next()
            .await
            .expect("stream is empty")
            .expect("failed to poll job")
            .expect("no job is pending")
    }

    fn example_email() -> Email {
        Email {
            subject: "Test Subject".to_string(),
            to: "example@mysql".to_string(),
            text: "Some Text".to_string(),
        }
    }

    struct DummyService {}

    async fn register_worker_at(
        storage: &mut MysqlStorage<Email>,
        last_seen: DateTime<Utc>,
    ) -> String {
        let worker_id = Uuid::new_v4().to_string();

        storage
            .keep_alive_at::<DummyService>(worker_id.clone(), last_seen)
            .await
            .expect("failed to register worker");
        worker_id
    }

    async fn register_worker(storage: &mut MysqlStorage<Email>) -> String {
        register_worker_at(storage, Utc::now()).await
    }

    async fn push_email<S>(storage: &mut S, email: Email)
    where
        S: Storage<Output = Email>,
    {
        storage.push(email).await.expect("failed to push a job");
    }

    async fn get_job<S>(storage: &mut S, job_id: String) -> JobRequest<Email>
    where
        S: Storage<Output = Email>,
    {
        storage
            .fetch_by_id(job_id)
            .await
            .expect("failed to fetch job by id")
            .expect("no job found by id")
    }

    #[tokio::test]
    async fn test_consume_last_pushed_job() {
        let mut storage = setup().await;
        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker(&mut storage).await;

        let job = consume_one(&mut storage, &worker_id).await;

        assert_eq!(*job.context().status(), JobState::Running);
        assert_eq!(*job.context().lock_by(), Some(worker_id.clone()));
        assert!(job.context().lock_at().is_some());

        cleanup(storage, worker_id).await;
    }

    #[tokio::test]
    async fn test_acknowledge_job() {
        let mut storage = setup().await;
        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker(&mut storage).await;

        let job = consume_one(&mut storage, &worker_id).await;
        let job_id = job.context().id();

        storage
            .ack(worker_id.clone(), job_id.clone())
            .await
            .expect("failed to acknowledge the job");

        let job = get_job(&mut storage, job_id.clone()).await;
        assert_eq!(*job.context().status(), JobState::Done);
        assert!(job.context().done_at().is_some());

        cleanup(storage, worker_id).await;
    }

    #[tokio::test]
    async fn test_kill_job() {
        let mut storage = setup().await;

        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker(&mut storage).await;

        let job = consume_one(&mut storage, &worker_id).await;
        let job_id = job.context().id();

        storage
            .kill(worker_id.clone(), job_id.clone())
            .await
            .expect("failed to kill job");

        let job = get_job(&mut storage, job_id.clone()).await;
        assert_eq!(*job.context().status(), JobState::Killed);
        assert!(job.context().done_at().is_some());

        cleanup(storage, worker_id).await;
    }

    #[tokio::test]
    async fn test_storage_heartbeat_reenqueuorphaned_pulse_last_seen_6min() {
        let mut storage = setup().await;

        // push an Email job
        storage
            .push(example_email())
            .await
            .expect("failed to push job");

        // register a worker not responding since 6 minutes ago
        let worker_id = Uuid::new_v4().to_string();
        let last_seen = Utc::now().sub(chrono::Duration::minutes(6));
        storage
            .keep_alive_at::<Email>(worker_id.clone(), last_seen)
            .await
            .unwrap();

        // fetch job
        let job = consume_one(&mut storage, &worker_id).await;
        assert_eq!(*job.context().status(), JobState::Running);

        // heartbeat with ReenqueueOrpharned pulse
        storage
            .heartbeat(StorageWorkerPulse::RenqueueOrpharned { count: 5 })
            .await
            .unwrap();

        // then, the job status has changed to Pending
        let job = storage.fetch_by_id(job.id()).await.unwrap().unwrap();
        let context = job.context();
        assert_eq!(*context.status(), JobState::Pending);
        assert!(context.lock_by().is_none());
        assert!(context.lock_at().is_none());
        assert!(context.done_at().is_none());
        assert_eq!(*context.last_error(), Some("Job was abandoned".to_string()));

        cleanup(storage, worker_id).await;
    }

    #[tokio::test]
    async fn test_storage_heartbeat_reenqueuorphaned_pulse_last_seen_4min() {
        let mut storage = setup().await;

        // push an Email job
        storage
            .push(example_email())
            .await
            .expect("failed to push job");

        // register a worker responding at 4 minutes ago
        let worker_id = Uuid::new_v4().to_string();
        let last_seen = Utc::now().sub(chrono::Duration::minutes(4));
        storage
            .keep_alive_at::<Email>(worker_id.clone(), last_seen)
            .await
            .unwrap();

        // fetch job
        let job = consume_one(&mut storage, &worker_id).await;
        assert_eq!(*job.context().status(), JobState::Running);

        // heartbeat with ReenqueueOrpharned pulse
        storage
            .heartbeat(StorageWorkerPulse::RenqueueOrpharned { count: 5 })
            .await
            .unwrap();

        // then, the job status is not changed
        let job = storage.fetch_by_id(job.id()).await.unwrap().unwrap();
        let context = job.context();
        assert_eq!(*context.status(), JobState::Running);
        assert_eq!(*context.lock_by(), Some(worker_id.clone()));

        cleanup(storage, worker_id).await;
    }
}

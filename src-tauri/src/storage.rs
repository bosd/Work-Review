use chrono::{Duration, Local, NaiveDate};
use std::fs;
use std::path::{Path, PathBuf};
use work_review_core::config::StorageConfig;
use work_review_core::database::Database;
use work_review_core::error::{AppError, Result};

/// 递归遍历目录（替代 walkdir）
fn walk_dir_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk_dir_recursive(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// 存储管理器
/// 负责清理过期的截图和数据
pub struct StorageManager {
    data_dir: PathBuf,
    config: StorageConfig,
}

impl StorageManager {
    /// 创建存储管理器
    pub fn new(data_dir: &Path, config: StorageConfig) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            config,
        }
    }

    /// 更新配置
    pub fn update_config(&mut self, config: StorageConfig) {
        self.config = config;
    }

    /// 执行清理任务
    pub fn cleanup(&self, database: &Database) -> Result<CleanupResult> {
        // 元数据清理须由用户显式开启；先完成数据库事务，失败时不继续删文件。
        self.cleanup_old_metadata(database)?;

        // 1. 清理过期的截图文件
        let mut screenshots_deleted = self.cleanup_old_screenshots()?;

        // 2. 清理过期的 OCR 日志文件
        let ocr_logs_deleted = self.cleanup_old_ocr_logs()?;

        // 3. 检查存储空间是否超限
        let current_size = self.calculate_storage_size()?;

        // 如果超过限制，继续删除最旧的数据
        if current_size > (self.config.storage_limit_mb as u64 * 1024 * 1024) {
            screenshots_deleted += self.cleanup_oldest_until_under_limit()?;
        }

        // 清理完成后重新统计占用，确保日志与返回结果反映清理后的实际大小
        let total_size_mb = self.calculate_storage_size()? as f64 / 1024.0 / 1024.0;

        log::info!(
            "存储清理完成: 删除 {screenshots_deleted} 个截图目录, {ocr_logs_deleted} 个 OCR 日志, 当前占用 {total_size_mb:.1} MB"
        );

        Ok(CleanupResult {
            screenshots_deleted,
            total_size_mb,
        })
    }

    fn cleanup_old_metadata(&self, database: &Database) -> Result<usize> {
        if !self.config.metadata_cleanup_enabled || self.config.metadata_retention_days == 0 {
            return Ok(0);
        }
        let cutoff = Local::now()
            .date_naive()
            .checked_sub_days(chrono::Days::new(u64::from(
                self.config.metadata_retention_days,
            )))
            .ok_or_else(|| AppError::Config("元数据保留天数超出有效范围".to_string()))?;
        let deleted =
            database.delete_activities_before_date(&cutoff.format("%Y-%m-%d").to_string())?;
        if deleted > 0 {
            log::info!("已清理 {cutoff} 之前的 {deleted} 条活动元数据及关联摘要");
        }
        Ok(deleted)
    }

    /// 清理过期截图
    fn cleanup_old_screenshots(&self) -> Result<u32> {
        // 0 表示"永久保留"——直接跳过基于时间的清理（存储上限那条另算）
        if self.config.screenshot_retention_days == 0 {
            return Ok(0);
        }

        let screenshots_dir = self.data_dir.join("screenshots");
        if !screenshots_dir.exists() {
            return Ok(0);
        }

        let cutoff_date = Local::now().date_naive()
            - Duration::days(self.config.screenshot_retention_days as i64);
        let mut deleted_count = 0u32;

        // 遍历日期目录
        for entry in fs::read_dir(&screenshots_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                // 目录名格式: YYYY-MM-DD
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    if let Ok(date) = NaiveDate::parse_from_str(dir_name, "%Y-%m-%d") {
                        if date < cutoff_date {
                            // 删除整个目录
                            match fs::remove_dir_all(&path) {
                                Ok(_) => {
                                    // 统计删除的文件数
                                    deleted_count += 1;
                                    log::info!("已删除过期截图目录: {dir_name}");
                                }
                                Err(e) => {
                                    log::warn!("删除目录失败 {dir_name}: {e}");
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(deleted_count)
    }

    /// 清理过期 OCR 日志文件
    /// 日志文件格式: ocr_logs/YYYY-MM-DD.txt，与截图使用相同的保留天数
    fn cleanup_old_ocr_logs(&self) -> Result<u32> {
        // 0 表示"永久保留"——与 cleanup_old_screenshots 保持一致
        if self.config.screenshot_retention_days == 0 {
            return Ok(0);
        }

        let ocr_dir = self.data_dir.join("ocr_logs");
        if !ocr_dir.exists() {
            return Ok(0);
        }

        let cutoff_date = Local::now().date_naive()
            - Duration::days(self.config.screenshot_retention_days as i64);
        let mut deleted_count = 0u32;

        for entry in fs::read_dir(&ocr_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                if let Some(stem) = path.file_stem().and_then(|n| n.to_str()) {
                    if let Ok(date) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") {
                        if date < cutoff_date {
                            match fs::remove_file(&path) {
                                Ok(_) => {
                                    deleted_count += 1;
                                    log::info!("已删除过期 OCR 日志: {stem}.txt");
                                }
                                Err(e) => {
                                    log::warn!("删除 OCR 日志失败 {stem}: {e}");
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(deleted_count)
    }

    /// 当存储超限时，删除最旧的数据直到低于限制
    fn cleanup_oldest_until_under_limit(&self) -> Result<u32> {
        let screenshots_dir = self.data_dir.join("screenshots");
        if !screenshots_dir.exists() {
            return Ok(0);
        }

        let limit_bytes = self.config.storage_limit_mb as u64 * 1024 * 1024;
        let mut current_size = self.calculate_storage_size()?;
        let mut deleted_count = 0u32;

        // 收集所有日期目录并排序
        let mut date_dirs: Vec<_> = fs::read_dir(&screenshots_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                    .map(|date| (date, e.path()))
            })
            .collect();

        // 按日期升序排序（最旧的在前）
        date_dirs.sort_by_key(|(date, _)| *date);

        // 删除最旧的目录直到低于限制（使用递减计算避免重复扫描）
        for (date, path) in date_dirs {
            if current_size < limit_bytes {
                break;
            }

            // 先计算该目录大小，删除后从总量中扣减
            let dir_size: u64 = walk_dir_recursive(&path)
                .iter()
                .filter_map(|p| fs::metadata(p).ok())
                .map(|m| m.len())
                .sum();

            match fs::remove_dir_all(&path) {
                Ok(_) => {
                    deleted_count += 1;
                    current_size = current_size.saturating_sub(dir_size);
                    log::info!(
                        "存储超限，已删除最旧目录: {date} (释放 {:.1} MB)",
                        dir_size as f64 / 1024.0 / 1024.0
                    );
                }
                Err(e) => {
                    log::warn!("删除目录失败 {date}: {e}");
                }
            }
        }

        Ok(deleted_count)
    }

    /// 计算当前存储占用大小（字节）
    fn calculate_storage_size(&self) -> Result<u64> {
        let screenshots_dir = self.data_dir.join("screenshots");
        if !screenshots_dir.exists() {
            return Ok(0);
        }

        let total_size: u64 = walk_dir_recursive(&screenshots_dir)
            .iter()
            .filter_map(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();

        Ok(total_size)
    }

    /// 获取存储统计信息
    pub fn get_stats(&self) -> Result<StorageStats> {
        let screenshots_dir = self.data_dir.join("screenshots");

        let mut stats = StorageStats::default();

        if !screenshots_dir.exists() {
            return Ok(stats);
        }

        // 遍历统计
        for path in walk_dir_recursive(&screenshots_dir) {
            if let Ok(metadata) = fs::metadata(&path) {
                stats.total_files += 1;
                stats.total_size_bytes += metadata.len();
            }
        }

        stats.total_size_mb = stats.total_size_bytes as f64 / 1024.0 / 1024.0;
        stats.storage_limit_mb = self.config.storage_limit_mb;
        stats.retention_days = self.config.screenshot_retention_days;

        Ok(stats)
    }
}

/// 清理结果
#[derive(Debug, Default)]
pub struct CleanupResult {
    pub screenshots_deleted: u32,
    pub total_size_mb: f64,
}

/// 存储统计信息
#[derive(Debug, Default, serde::Serialize)]
pub struct StorageStats {
    pub total_files: u64,
    pub total_size_bytes: u64,
    pub total_size_mb: f64,
    pub storage_limit_mb: u32,
    pub retention_days: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use work_review_core::database::Activity;

    #[test]
    fn 元数据清理须显式开启且保留边界日期与永久保留数据() {
        let dir =
            std::env::temp_dir().join(format!("work-review-retention-{}", uuid::Uuid::new_v4()));
        let db = Database::new(&dir.join("test.db")).unwrap();
        let cutoff = Local::now().date_naive() - Duration::days(30);
        let boundary = Local
            .from_local_datetime(&cutoff.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp();
        let insert = |timestamp| {
            db.insert_activity(&Activity {
                id: None,
                timestamp,
                app_name: "Editor".into(),
                window_title: "Private project".into(),
                screenshot_path: String::new(),
                ocr_text: Some("Private OCR".into()),
                category: "development".into(),
                duration: 1,
                browser_url: Some("https://example.org".into()),
                executable_path: None,
                semantic_category: None,
                semantic_confidence: None,
                screenshot_url: None,
            })
            .unwrap()
        };
        let old_id = insert(boundary - 1);
        let boundary_id = insert(boundary);
        let recent_id = insert(Local::now().timestamp());
        let mut config = StorageConfig::default();
        let mut manager = StorageManager::new(&dir, config.clone());

        manager.cleanup(&db).unwrap();
        assert!(
            db.get_activity_by_id(old_id).unwrap().is_some(),
            "升级后默认不得自动删除历史"
        );
        config.metadata_cleanup_enabled = true;
        config.metadata_retention_days = 0;
        manager.update_config(config.clone());
        manager.cleanup(&db).unwrap();
        assert!(
            db.get_activity_by_id(old_id).unwrap().is_some(),
            "0 表示永久保留"
        );

        config.metadata_retention_days = u32::MAX;
        manager.update_config(config.clone());
        assert!(
            manager.cleanup(&db).is_err(),
            "非法保留天数应安全失败，不得误删或panic"
        );
        assert!(db.get_activity_by_id(old_id).unwrap().is_some());

        config.metadata_retention_days = 30;
        manager.update_config(config);
        manager.cleanup(&db).unwrap();
        assert!(db.get_activity_by_id(old_id).unwrap().is_none());
        assert!(db.get_activity_by_id(boundary_id).unwrap().is_some());
        assert!(db.get_activity_by_id(recent_id).unwrap().is_some());
        manager.cleanup(&db).unwrap();
        assert!(db.get_activity_by_id(recent_id).unwrap().is_some());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

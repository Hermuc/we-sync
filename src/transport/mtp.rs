//! MTP/WPD 通道：手机以「文件传输(MTP)」方式接入，无需 USB 调试。
//!
//! 实现要点（WPD）：
//! * CLSID_PortableDeviceManager 枚举设备 → CLSID_PortableDevice::Open
//! * IPortableDeviceContent::EnumObjects 逐级遍历对象树找目标目录
//! * CreateObjectWithPropertiesAndData 拿 IStream 写入流完成上传
//! * 目录不存在时用 CreateObjectWithPropertiesOnly 逐级创建

use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{HSTRING, PCWSTR, PWSTR, PROPVARIANT};
use windows::Win32::Devices::PortableDevices::*;
use windows::Win32::Foundation::S_OK;
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IStream, CLSCTX_INPROC_SERVER, STGC_DEFAULT,
};

use super::{Channel, Transport};

pub struct MtpTransport {
    content: IPortableDeviceContent,
    props: IPortableDeviceProperties,
    name: String,
    /// (显示名, 对象 ID)
    storages: Vec<(String, String)>,
    storage_index: usize,
    /// remote_dir -> 已解析到的父目录对象 ID
    dir_cache: std::collections::HashMap<String, String>,
}

impl MtpTransport {
    /// 枚举设备并打开第一个可用的 WPD 设备（通常是手机的 MTP）。
    pub fn detect(storage_index: usize) -> Option<Self> {
        unsafe { Self::detect_inner(storage_index) }.ok().flatten()
    }

    unsafe fn detect_inner(storage_index: usize) -> Result<Option<Self>> {
        let manager: IPortableDeviceManager =
            CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)?;

        let mut count = 0u32;
        manager.GetDevices(std::ptr::null_mut(), &mut count)?;
        if count == 0 {
            return Ok(None);
        }
        let mut ids: Vec<PWSTR> = vec![PWSTR::null(); count as usize];
        manager.GetDevices(ids.as_mut_ptr(), &mut count)?;

        let mut result = None;
        for id in ids.iter().take(count as usize) {
            if id.is_null() {
                continue;
            }
            let id_str = id.to_string().unwrap_or_default();
            match Self::open_device(PCWSTR(id.as_ptr()), storage_index) {
                Ok(Some(t)) => {
                    crate::log_info!("MTP 设备已就绪: {} (存储: {})", t.name, t.storages.iter().map(|s| s.0.clone()).collect::<Vec<_>>().join(", "));
                    result = Some(t);
                    break;
                }
                Ok(None) => crate::log_debug!("跳过 WPD 设备(无 storage): {id_str}"),
                Err(e) => crate::log_debug!("打开 WPD 设备失败({id_str}): {e}"),
            }
        }

        for id in ids.iter().take(count as usize) {
            if !id.is_null() {
                CoTaskMemFree(Some(id.as_ptr() as *const c_void));
            }
        }
        Ok(result)
    }

    unsafe fn open_device(device_id: PCWSTR, storage_index: usize) -> Result<Option<Self>> {
        let device: IPortableDevice =
            CoCreateInstance(&PortableDevice, None, CLSCTX_INPROC_SERVER)?;
        let client: IPortableDeviceValues =
            CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
        client.SetStringValue(&WPD_CLIENT_NAME, &HSTRING::from("WE-Sync"))?;
        client.SetUnsignedIntegerValue(&WPD_CLIENT_MAJOR_VERSION, 1)?;
        client.SetUnsignedIntegerValue(&WPD_CLIENT_MINOR_VERSION, 0)?;
        client.SetUnsignedIntegerValue(&WPD_CLIENT_REVISION, 0)?;
        // SECURITY_IMPERSONATION = 2，某些机型缺这个会导致创建对象失败
        client.SetUnsignedIntegerValue(&WPD_CLIENT_SECURITY_QUALITY_OF_SERVICE, 2)?;
        device.Open(device_id, &client)?;

        let content = device.Content()?;
        let props = content.Properties()?;

        // 第一层对象即 storage
        let mut storages: Vec<(String, String)> = Vec::new();
        for oid in enumerate_objects(&content, "DEVICE")? {
            let name = read_entry(&props, &oid)
                .map(|e| e.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| oid.clone());
            storages.push((name, oid));
        }
        if storages.is_empty() {
            return Ok(None);
        }

        // 设备友好名
        let mut name = String::from("Android (MTP)");
        let manager: IPortableDeviceManager =
            CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)?;
        let mut len = 0u32;
        if manager
            .GetDeviceFriendlyName(device_id, PWSTR::null(), &mut len)
            .is_ok()
            && len > 0
        {
            let mut buf = vec![0u16; len as usize];
            let mut len2 = len;
            if manager
                .GetDeviceFriendlyName(device_id, PWSTR(buf.as_mut_ptr()), &mut len2)
                .is_ok()
            {
                let s = String::from_utf16_lossy(&buf[..(len2 as usize).saturating_sub(1)]);
                if !s.trim().is_empty() {
                    name = format!("{} (MTP)", s.trim());
                }
            }
        }

        let idx = storage_index.min(storages.len() - 1);
        Ok(Some(Self {
            content,
            props,
            name,
            storages,
            storage_index: idx,
            dir_cache: Default::default(),
        }))
    }

    fn storage_root(&self) -> &str {
        &self.storages[self.storage_index].1
    }

    /// 把 Android 路径转成相对 storage 根的组件列表。
    fn remote_components(remote_dir: &str) -> Vec<String> {
        let mut p = remote_dir.trim().replace('\\', "/");
        for prefix in ["/storage/emulated/0", "/sdcard", "/mnt/sdcard", "/storage/self/primary"] {
            if p == prefix || p.starts_with(&format!("{prefix}/")) {
                p = p[prefix.len()..].to_string();
                break;
            }
        }
        p.split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    /// 找到或（按需）创建目标目录，返回其对象 ID。
    fn resolve_dir(&mut self, remote_dir: &str, create: bool) -> Result<String> {
        if let Some(id) = self.dir_cache.get(remote_dir) {
            return Ok(id.clone());
        }
        let mut current = self.storage_root().to_string();
        for comp in Self::remote_components(remote_dir) {
            let entries = self.list_entries(&current)?;
            let mut found = None;
            for e in entries {
                if e.is_folder && e.name.eq_ignore_ascii_case(&comp) {
                    found = Some(e.id);
                    break;
                }
            }
            current = match found {
                Some(id) => id,
                None if create => self.create_folder(&current, &comp)?,
                None => return Err(anyhow!("远程目录不存在: {remote_dir}")),
            };
        }
        self.dir_cache.insert(remote_dir.to_string(), current.clone());
        Ok(current)
    }

    /// 一次属性往返拿全（名字/大小/是否目录），避免逐个对象多次 IPC。
    fn list_entries(&self, parent: &str) -> Result<Vec<Entry>> {
        let ids = enumerate_objects(&self.content, parent)?;
        let mut out = Vec::with_capacity(ids.len());
        for oid in ids {
            match read_entry(&self.props, &oid) {
                Some(e) => out.push(e),
                None => out.push(Entry {
                    id: oid,
                    name: String::new(),
                    is_folder: false,
                    size: 0,
                }),
            }
        }
        Ok(out)
    }

    fn create_folder(&self, parent_id: &str, name: &str) -> Result<String> {
        unsafe {
            let values: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
            let parent = HSTRING::from(parent_id);
            values.SetStringValue(&WPD_OBJECT_PARENT_ID, &parent)?;
            let n = HSTRING::from(name);
            values.SetStringValue(&WPD_OBJECT_NAME, &n)?;
            values.SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, &n)?;
            values.SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_FOLDER)?;
            let mut new_id = PWSTR::null();
            self.content
                .CreateObjectWithPropertiesOnly(&values, &mut new_id)?;
            let id = new_id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(new_id.as_ptr() as *const c_void));
            crate::log_info!("已在手机上创建目录: {name}");
            Ok(id)
        }
    }

    fn find_file(&mut self, parent_id: &str, name: &str) -> Option<(String, u64)> {
        let entries = self.list_entries(parent_id).ok()?;
        entries
            .into_iter()
            .find(|e| !e.is_folder && e.name.eq_ignore_ascii_case(name))
            .map(|e| (e.id, e.size))
    }
}

impl Transport for MtpTransport {
    fn channel(&self) -> Channel {
        Channel::Mtp
    }

    fn device_name(&self) -> String {
        self.name.clone()
    }

    fn storages(&self) -> Vec<String> {
        self.storages.iter().map(|s| s.0.clone()).collect()
    }

    fn ping(&mut self) -> Result<()> {
        // 只发起一次 EnumObjects 就返回，不遍历、不读属性：手机拔线/切「仅充电」后
        // WPD 会话上的任意调用都会返回错误码（E_DEVICE_NOT_CONNECTED 等），够用且便宜。
        let root = self.storage_root().to_string();
        unsafe {
            self.content.EnumObjects(0, &HSTRING::from(root), None)?;
        }
        Ok(())
    }

    fn ensure_dir(&mut self, remote_dir: &str) -> Result<()> {
        self.resolve_dir(remote_dir, true).map(|_| ())
    }

    fn list_files(&mut self, remote_dir: &str) -> Result<Vec<(String, u64)>> {
        let parent = match self.resolve_dir(remote_dir, false) {
            Ok(id) => id,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(self
            .list_entries(&parent)?
            .into_iter()
            .filter(|e| !e.is_folder && !e.name.is_empty())
            .map(|e| (e.name, e.size))
            .collect())
    }

    fn upload(
        &mut self,
        local: &Path,
        remote_dir: &str,
        remote_name: &str,
        abort: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<()> {
        let parent_id = self.resolve_dir(remote_dir, true)?;
        let total = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);

        // 同名同大小已存在 => 复用（避免重复占用空间）
        if let Some((existing_id, existing_size)) = self.find_file(&parent_id, remote_name) {
            if existing_size == total {
                crate::log_debug!("手机上已存在同大小文件，跳过: {remote_name}");
                progress(total, total);
                return Ok(());
            }
            crate::log_debug!("手机上存在不同大小的同名文件，先删除: {remote_name}");
            let _ = self.delete_by_id(&existing_id);
        }

        // 先按「视频对象」写；部分机型对 VIDEO 类型支持不好，退回「通用文件」
        let first = self.write_object(
            &parent_id,
            remote_name,
            local,
            total,
            &WPD_CONTENT_TYPE_VIDEO,
            Some(&WPD_OBJECT_FORMAT_MP4),
            abort,
            progress,
        );
        first.or_else(|e| {
                if super::is_paused(&e) {
                    return Err(e);
                }
                crate::log_warn!("以视频类型写入失败({e})，改用通用文件重试");
                if let Some((oid, _)) = self.find_file(&parent_id, remote_name) {
                    let _ = self.delete_by_id(&oid);
                }
                self.write_object(
                    &parent_id,
                    remote_name,
                    local,
                    total,
                    &WPD_CONTENT_TYPE_GENERIC_FILE,
                    None,
                    abort,
                    progress,
                )
        })
    }
}

impl MtpTransport {
    #[allow(clippy::too_many_arguments)]
    fn write_object(
        &mut self,
        parent_id: &str,
        remote_name: &str,
        local: &Path,
        total: u64,
        content_type: &windows::core::GUID,
        format: Option<&windows::core::GUID>,
        abort: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<()> {
        unsafe {
            let values: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
            let parent = HSTRING::from(parent_id);
            values.SetStringValue(&WPD_OBJECT_PARENT_ID, &parent)?;
            let n = HSTRING::from(remote_name);
            values.SetStringValue(&WPD_OBJECT_NAME, &n)?;
            values.SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, &n)?;
            values.SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, content_type)?;
            if let Some(f) = format {
                values.SetGuidValue(&WPD_OBJECT_FORMAT, f)?;
            }
            values.SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, total)?;

            let mut stream: Option<IStream> = None;
            let mut optimal = 0u32;
            self.content.CreateObjectWithPropertiesAndData(
                &values,
                &mut stream,
                &mut optimal,
                std::ptr::null_mut(),
            )?;
            let stream = stream.ok_or_else(|| anyhow!("WPD 未返回写入流"))?;

            let chunk = if optimal == 0 { 256 * 1024 } else { optimal as usize };
            let mut file = std::fs::File::open(local)?;
            let mut buf = vec![0u8; chunk.max(64 * 1024)];
            let mut written_total: u64 = 0;
            use std::io::Read;
            loop {
                if abort.load(Ordering::Relaxed) {
                    drop(stream);
                    // 不 Commit：丢弃半截对象；万一部分设备已落盘则显式删除
                    if let Some((oid, _)) = self.find_file(parent_id, remote_name) {
                        let _ = self.delete_by_id(&oid);
                    }
                    crate::log_info!("已暂停并清理半截文件: {remote_name}");
                    return Err(super::paused());
                }
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                let mut written = 0u32;
                let hr = stream.Write(buf.as_ptr() as *const c_void, n as u32, Some(&mut written));
                if hr != S_OK || written as usize != n {
                    return Err(anyhow!("写入失败: {hr:?} ({written}/{n})"));
                }
                written_total += n as u64;
                progress(written_total, total);
            }
            stream.Commit(STGC_DEFAULT)?;
            progress(total, total);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- WPD 小工具

fn enumerate_objects(content: &IPortableDeviceContent, parent: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    unsafe {
        let enumr = content.EnumObjects(0, &HSTRING::from(parent), None)?;
        loop {
            let mut ids: Vec<PWSTR> = vec![PWSTR::null(); 16];
            let mut fetched = 0u32;
            let hr = enumr.Next(ids.as_mut_slice(), &mut fetched);
            if hr.is_err() || fetched == 0 {
                // 释放已分配的（若有）
                for id in ids.iter().take(fetched as usize) {
                    if !id.is_null() {
                        CoTaskMemFree(Some(id.as_ptr() as *const c_void));
                    }
                }
                break;
            }
            for id in ids.iter().take(fetched as usize) {
                if id.is_null() {
                    continue;
                }
                out.push(id.to_string().unwrap_or_default());
                CoTaskMemFree(Some(id.as_ptr() as *const c_void));
            }
            if fetched < 16 {
                break;
            }
        }
    }
    Ok(out)
}

/// 一个 WPD 对象的常用属性。
pub struct Entry {
    pub id: String,
    pub name: String,
    pub is_folder: bool,
    pub size: u64,
}

/// 一次 GetValues 拿齐名字/大小/类型，减少 MTP 往返。
fn read_entry(props: &IPortableDeviceProperties, oid: &str) -> Option<Entry> {
    unsafe {
        let keys: IPortableDeviceKeyCollection =
            CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER).ok()?;
        for k in [
            &WPD_OBJECT_ORIGINAL_FILE_NAME,
            &WPD_OBJECT_NAME,
            &WPD_OBJECT_SIZE,
            &WPD_OBJECT_CONTENT_TYPE,
        ] {
            if keys.Add(k).is_err() {
                return None;
            }
        }
        let values = props.GetValues(&HSTRING::from(oid), &keys).ok()?;

        let mut name = String::new();
        for k in [&WPD_OBJECT_ORIGINAL_FILE_NAME, &WPD_OBJECT_NAME] {
            if let Ok(p) = values.GetStringValue(k) {
                if !p.is_null() {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.as_ptr() as *const c_void));
                    if !s.is_empty() {
                        name = s;
                        break;
                    }
                }
            }
        }

        let size = values.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE).unwrap_or(0);
        let ct = values.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE).unwrap_or_default();
        Some(Entry {
            id: oid.to_string(),
            name,
            is_folder: ct == WPD_CONTENT_TYPE_FOLDER,
            size,
        })
    }
}

impl MtpTransport {
    fn delete_by_id(&mut self, object_id: &str) -> Result<()> {
        // WPD 删除需要 IPortableDevicePropVariantCollection，这里用 Move 到临时目录更麻烦，
        // 直接调用 Delete 需要 VT_LPWSTR 集合；失败也不影响后续覆盖写。
        unsafe {
            let ids: IPortableDevicePropVariantCollection = CoCreateInstance(
                &PortableDevicePropVariantCollection,
                None,
                CLSCTX_INPROC_SERVER,
            )?;
            let v = PROPVARIANT::from(object_id);
            ids.Add(&v)?;
            let mut results: Option<IPortableDevicePropVariantCollection> = None;
            self.content.Delete(0, &ids, &mut results)?;
        }
        Ok(())
    }
}

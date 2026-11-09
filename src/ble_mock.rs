use anyhow::{bail, Result};
use btleplug::api::{
    CharPropFlags, Characteristic, ScanFilter, Service, ValueNotification, WriteType,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
#[derive(Default)]
pub struct Control {
    pub fail: Option<&'static str>,
    pub connected: bool,
    pub calls: Vec<String>,
}
#[derive(Clone, Default)]
pub struct Peripheral(pub Arc<Mutex<Control>>);
pub struct Properties {
    pub address: String,
    pub local_name: Option<String>,
    pub rssi: Option<i16>,
}
impl Peripheral {
    fn op(&self, name: &str) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        c.calls.push(name.into());
        if c.fail == Some(name) {
            bail!("simulated {name}");
        }
        Ok(())
    }
    pub async fn properties(&self) -> Result<Option<Properties>> {
        self.op("properties")?;
        Ok(Some(Properties {
            address: "AA:BB:CC:DD:EE:FF".into(),
            local_name: Some("mock".into()),
            rssi: Some(-40),
        }))
    }
    pub async fn connect(&self) -> Result<()> {
        self.op("connect")?;
        self.0.lock().unwrap().connected = true;
        Ok(())
    }
    pub async fn disconnect(&self) -> Result<()> {
        self.op("disconnect")?;
        self.0.lock().unwrap().connected = false;
        Ok(())
    }
    pub async fn is_connected(&self) -> Result<bool> {
        self.op("is_connected")?;
        Ok(self.0.lock().unwrap().connected)
    }
    pub async fn discover_services(&self) -> Result<()> {
        self.op("discover")
    }
    pub fn services(&self) -> BTreeSet<Service> {
        let mut characteristics = BTreeSet::new();
        characteristics.insert(Characteristic {
            uuid: uuid::Uuid::nil(),
            service_uuid: uuid::Uuid::nil(),
            properties: CharPropFlags::READ | CharPropFlags::WRITE | CharPropFlags::NOTIFY,
            descriptors: BTreeSet::new(),
        });
        [Service {
            uuid: uuid::Uuid::nil(),
            primary: true,
            characteristics,
        }]
        .into_iter()
        .collect()
    }
    pub async fn notifications(&self) -> Result<futures::stream::Pending<ValueNotification>> {
        self.op("notifications")?;
        Ok(futures::stream::pending())
    }
    pub async fn read(&self, _: &Characteristic) -> Result<Vec<u8>> {
        self.op("read")?;
        Ok(vec![0, 255])
    }
    pub async fn write(&self, _: &Characteristic, _: &[u8], kind: WriteType) -> Result<()> {
        self.op(match kind {
            WriteType::WithResponse => "write_response",
            _ => "write_no_response",
        })
    }
    pub async fn subscribe(&self, _: &Characteristic) -> Result<()> {
        self.op("subscribe")
    }
    pub async fn unsubscribe(&self, _: &Characteristic) -> Result<()> {
        self.op("unsubscribe")
    }
}
#[derive(Clone, Default)]
pub struct Adapter(pub Peripheral);
impl Adapter {
    pub async fn adapter_info(&self) -> Result<String> {
        Ok("mock".into())
    }
    pub async fn start_scan(&self, _: ScanFilter) -> Result<()> {
        self.0.op("start_scan")
    }
    pub async fn stop_scan(&self) -> Result<()> {
        self.0.op("stop_scan")
    }
    pub async fn peripherals(&self) -> Result<Vec<Peripheral>> {
        self.0.op("peripherals")?;
        Ok(vec![self.0.clone()])
    }
}
pub struct Manager;
impl Manager {
    pub async fn new() -> Result<Self> {
        Ok(Self)
    }
    pub async fn adapters(&self) -> Result<Vec<Adapter>> {
        Ok(vec![Adapter::default()])
    }
}

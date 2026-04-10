pub struct U48(u64);

impl U48 {
    pub fn to_le_bytes(&self) -> [u8; 6] {
        let byts = self.0.to_le_bytes();
        [byts[0], byts[1], byts[2], byts[3], byts[4], byts[5]]
    }
}

pub struct ResourceInfoRequest {
    pub request_id: u32,
    pub resource_id: u32,
}

pub struct ResourceInfoResponse<'a> {
    pub request_id: u32,
    pub content_type: &'a str,
    pub resource_size: ResourceSize,
}

pub enum ResourceSize {
    Unknown,
    Known(U48),
}

pub struct ResourceRequest {
    pub request_id: u32,
    pub resource_id: u32,
    pub read_head: ReadHead,
}

pub struct ResourceResponse<'a> {
    pub request_id: u32,
    pub result: GetResourceResult<'a>,
}

pub enum ReadHead {
    Whole,
    Range { start: U48, stop_inclusive: U48 },
}

pub enum GetResourceResult<'a> {
    None,
    Success(&'a [u8]),
}

pub fn create_url(provider_id: u16, resource_id: u32) -> String {
    format!("fcomp://{provider_id}.fcast/{resource_id}")
}

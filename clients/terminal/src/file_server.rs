struct FileServer {
    base_url: String,
    base_path: String,
}

impl FileServer {
    fn new(base_url: String, base_path: String) -> Self {
        FileServer { base_url, base_path }
    }

    async fn serve(&self) {
        let file_server = warp::fs::dir(&self.base_path);
        warp::serve(file_server).run(([127, 0, 0, 1], 3030)).await;
    }

    fn get_url(&self, file_name: &str) -> String {
        format!("{}/{}", self.base_url, file_name)
    }
}

pub async fn host_file_and_get_url(file_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let file_name = Path::new(file_path).file_name().ok_or("Invalid file path")?.to_str().ok_or("Invalid file name")?;
    let file_server = FileServer::new("http://127.0.0.1:3030".to_string(), "path/to/hosted/files".to_string());

    // Copy the file to the hosting directory
    let destination = Path::new(&file_server.base_path).join(file_name);
    tokio::fs::copy(file_path, &destination).await?;

    // Start the server if not already running
    // This part needs synchronization in a real-world scenario
    tokio::spawn(async move {
        file_server.serve().await;
    });

    Ok(file_server.get_url(file_name))
}
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() == Some("windows") {
        let mut res = winres::WindowsResource::new();

        res.set_icon("icons/zedis.ico");
        if !std::env::var("HOST")
            .map(|host| host.contains("windows"))
            .unwrap_or(false)
        {
            eprintln!("Embedding Windows common-controls manifest for cross compilation");
            res.set_manifest(
                r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <asmv3:application>
        <asmv3:windowsSettings>
            <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true</dpiAware>
            <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
        </asmv3:windowsSettings>
    </asmv3:application>
    <dependency>
        <dependentAssembly>
            <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" />
        </dependentAssembly>
    </dependency>
</assembly>"#,
            );
        }

        if let Err(e) = res.compile() {
            eprintln!("Failed to compile Windows resources: {}", e);
            std::process::exit(1);
        }
        if std::env::var("CARGO_CFG_TARGET_ENV").ok().as_deref() == Some("gnu")
            && let Ok(out_dir) = std::env::var("OUT_DIR")
        {
            println!("cargo:rustc-link-arg-bin=zedis={out_dir}/resource.o");
        }
    }
}

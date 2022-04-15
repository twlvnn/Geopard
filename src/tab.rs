use anyhow::{bail, Context, Result};
use async_fs::File;
use futures::future::RemoteHandle;
use futures::io::BufReader;
use futures::prelude::*;
use futures::task::LocalSpawnExt;
use glib_macros::Properties;
use gtk::gdk::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use log::{debug, error, info};
use once_cell::sync::Lazy;
use std::cell::RefCell;
use url::Url;

use crate::common;
use crate::common::{glibctx, HistoryItem, LossyTextRead, PageElement, RequestCtx};
use crate::draw_ctx::DrawCtx;
use crate::gemini;

pub mod imp {

    pub use super::*;
    #[derive(Debug, Default, Properties)]
    pub struct Tab {
        pub(crate) gemini_client: RefCell<gemini::Client>,
        pub(crate) draw_ctx: RefCell<Option<DrawCtx>>,
        pub(crate) history: RefCell<Vec<HistoryItem>>,
        pub(crate) scroll_win: gtk::ScrolledWindow,
        pub(crate) clamp: adw::Clamp,
        pub(crate) left_click_ctrl: RefCell<Option<gtk::GestureClick>>,
        pub(crate) right_click_ctrl: RefCell<Option<gtk::GestureClick>>,
        pub(crate) req_handle: RefCell<Option<RemoteHandle<()>>>,
        #[prop(get, set)]
        pub(crate) progress: RefCell<f64>,
        #[prop(get)]
        pub(crate) title: RefCell<String>,
        #[prop(get)]
        pub(crate) url: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Tab {
        const NAME: &'static str = "GeopardTab";
        type Type = super::Tab;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            // The layout manager determines how child widgets are laid out.
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for Tab {
        fn constructed(&self, obj: &Self::Type) {
            self.parent_constructed(obj);

            self.scroll_win.set_vexpand(true);
            obj.set_vexpand(true);

            self.clamp.set_parent(obj);
            self.clamp.set_maximum_size(768);
            self.clamp.set_tightening_threshold(720);
            self.clamp.set_child(Some(&self.scroll_win));

            self.left_click_ctrl
                .replace(Some(gtk::GestureClick::builder().button(1).build()));
            self.right_click_ctrl
                .replace(Some(gtk::GestureClick::builder().button(3).build()));
            self.gemini_client
                .replace(gemini::ClientBuilder::new().redirect(true).build());
        }

        fn dispose(&self, _obj: &Self::Type) {
            self.clamp.unparent();
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
                vec![glib::subclass::Signal::builder(
                    "open-background-tab",
                    &[],
                    <()>::static_type().into(),
                )
                .build()]
            });
            SIGNALS.as_ref()
        }

        fn properties() -> &'static [glib::ParamSpec] {
            Self::derived_properties()
        }

        fn set_property(
            &self,
            obj: &Self::Type,
            id: usize,
            value: &glib::Value,
            pspec: &glib::ParamSpec,
        ) {
            self.derived_set_property(obj, id, value, pspec).unwrap()
        }

        fn property(&self, obj: &Self::Type, id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            self.derived_property(obj, id, pspec).unwrap()
        }
    }
    impl WidgetImpl for Tab {}
}
glib::wrapper! {
    pub struct Tab(ObjectSubclass<imp::Tab>)
        @extends gtk::Widget;
}
pub use imp::TabPropertiesExt;
impl Tab {
    pub fn new(config: crate::config::Config) -> Self {
        let this: Self = glib::Object::new(&[]).unwrap();
        let imp = this.imp();
        use common::MARGIN;
        let text_view = gtk::builders::TextViewBuilder::new()
            .top_margin(MARGIN * 2)
            .left_margin(MARGIN)
            .right_margin(MARGIN)
            .bottom_margin(MARGIN * 4)
            .indent(2)
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        text_view.add_controller(imp.left_click_ctrl.borrow().as_ref().unwrap());
        text_view.add_controller(imp.right_click_ctrl.borrow().as_ref().unwrap());

        imp.scroll_win.set_child(Some(&text_view));
        imp.draw_ctx.replace(Some(DrawCtx::new(text_view, config)));

        this.bind_signals();
        this
    }
    pub fn child(&self) -> &gtk::ScrolledWindow {
        let imp = self.imp();
        &imp.scroll_win
    }
    pub fn build_request_ctx(&self, url: Url) -> RequestCtx {
        let imp = self.imp();
        RequestCtx {
            draw_ctx: imp.draw_ctx.borrow().clone().unwrap(),
            gemini_client: imp.gemini_client.borrow().clone(),
            url,
        }
    }
    pub fn url(&self) -> Result<url::Url> {
        let imp = self.imp();
        Ok(imp
            .history
            .borrow_mut()
            .last()
            .context("No items in history")?
            .url
            .clone())
    }
    pub fn add_cache(&self, cache: Vec<u8>) {
        let imp = self.imp();
        if let Some(item) = imp.history.borrow_mut().last_mut() {
            item.cache = Some(cache)
        }
    }
    pub fn handle_click(&self, x: f64, y: f64) -> Result<()> {
        let imp = self.imp();
        let draw_ctx = imp.draw_ctx.borrow();
        let text_view = &draw_ctx.as_ref().unwrap().text_view;
        let has_selection = text_view.buffer().has_selection();
        if has_selection {
            return Ok(());
        }
        let link = Self::extract_linkhandler(draw_ctx.as_ref().unwrap(), x, y)?;
        let url = self.parse_link(&link)?;
        self.spawn_open_url(url);
        Ok(())
    }
    fn handle_right_click(&self, x: f64, y: f64) -> Result<()> {
        let imp = self.imp();
        let draw_ctx = imp.draw_ctx.borrow();
        let text_view = &draw_ctx.as_ref().unwrap().text_view;
        let link = Self::extract_linkhandler(draw_ctx.as_ref().unwrap(), x, y)?;
        let link = self.parse_link(&link)?;

        let menu = gio::Menu::new();
        menu.insert(
            0,
            Some("Open Link In New Tab"),
            Some(&format!("win.open-in-new-tab(\"{}\")", link.as_str())),
        );
        menu.insert(
            1,
            Some("Copy Link"),
            Some(&format!("win.set-clipboard(\"{}\")", link.as_str())),
        );
        text_view.set_extra_menu(Some(&menu));
        Ok(())
    }
    fn spawn_request(&self, fut: impl Future<Output = ()> + 'static) {
        let imp = self.imp();
        imp.req_handle
            .replace(Some(glibctx().spawn_local_with_handle(fut).unwrap()));
    }
    pub fn spawn_open_url(&self, url: Url) {
        let imp = self.imp();

        self.set_progress(0.0);
        *imp.title.borrow_mut() = url.to_string();
        self.notify("title");
        *imp.url.borrow_mut() = url.to_string();
        self.notify("url");

        let scroll_progress = imp.scroll_win.vadjustment().value();
        let mut history = imp.history.borrow_mut();
        if let Some(item) = history.last_mut() {
            item.scroll_progress = scroll_progress;
        }
        history.push(HistoryItem {
            url: url.clone(),
            cache: None,
            scroll_progress: 0.0,
        });
        let mut req_ctx = self.build_request_ctx(url.clone());

        let this = self.clone();
        let fut = async move {
            match Self::send_request(&mut req_ctx).await {
                Ok(Some(cache)) => {
                    this.add_cache(cache);
                    info!("Page loaded and cached ({})", url.clone());
                }
                Ok(_) => {
                    info!("Page loaded ({})", url.clone());
                }
                Err(e) => {
                    Self::display_error(&mut req_ctx.draw_ctx, e);
                }
            }
            this.set_progress(1.0);
        };
        self.set_progress(0.3);
        self.spawn_request(fut);
    }
    fn spawn_open_history(&self, item: HistoryItem) {
        let HistoryItem { url, cache, .. } = item;
        match cache {
            Some(cache) => self.spawn_open_cached(url, cache),
            None => self.spawn_open_url(url),
        }
    }
    fn spawn_open_cached(&self, url: Url, cache: Vec<u8>) {
        let imp = self.imp();
        imp.history.borrow_mut().push(HistoryItem {
            url: url.clone(),
            cache: None,
            scroll_progress: 0.0,
        });

        let mut draw_ctx = imp.draw_ctx.borrow().clone().unwrap();
        let this = self.clone();
        *self.imp().title.borrow_mut() = url.to_string();
        self.notify("title");

        *self.imp().url.borrow_mut() = url.to_string();
        self.notify("url");

        let fut = async move {
            let buf = BufReader::new(cache.as_slice());
            draw_ctx.clear();
            let res = Self::display_gemini(&mut draw_ctx, buf).await;
            match res {
                Ok(cache) => {
                    info!("Loaded {} from cache", &url);
                    this.add_cache(cache);
                }
                Err(e) => Self::display_error(&mut draw_ctx, e),
            }
        };
        self.spawn_request(fut);
    }
    pub fn back(&self) -> Result<()> {
        let imp = self.imp();
        let item = {
            let mut history = imp.history.borrow_mut();
            if history.len() <= 1 {
                bail!("Already at last item in history");
            }
            history.pop();
            history.pop()
        };
        match item {
            Some(item) => self.spawn_open_history(item),
            None => unreachable!(),
        }
        Ok(())
    }
    pub fn display_error(ctx: &mut DrawCtx, error: anyhow::Error) {
        error!("{:?}", error);
        let error_text = format!("Geopard experienced an error:\n {:?}", error);
        ctx.insert_paragraph(&mut ctx.text_buffer.end_iter(), &error_text);
    }
    fn bind_signals(&self) {
        let imp = self.imp();
        let this = self.clone();
        let left_click_ctrl = imp.left_click_ctrl.borrow();
        let left_click_ctrl = left_click_ctrl.as_ref().unwrap();
        left_click_ctrl.connect_released(move |_ctrl, _n_press, x, y| {
            if let Err(e) = this.handle_click(x, y) {
                info!("{}", e);
            };
        });

        let this = self.clone();
        imp.right_click_ctrl
            .borrow()
            .as_ref()
            .unwrap()
            .connect_pressed(move |_ctrl, _n_press, x, y| {
                if let Err(e) = this.handle_right_click(x, y) {
                    info!("{}", e);
                };
            });
    }
    fn extract_linkhandler(draw_ctx: &DrawCtx, x: f64, y: f64) -> Result<String> {
        info!("Extracting linkhandler from clicked text");
        let text_view = &draw_ctx.text_view;
        let (x, y) =
            text_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let iter = text_view
            .iter_at_location(x as i32, y as i32)
            .context("Can't get text iter where clicked")?;

        for tag in iter.tags() {
            if let Some(link) = DrawCtx::linkhandler(&tag) {
                return Ok(link.clone());
            }
        }

        Err(anyhow::Error::msg("Clicked text doesn't have a link tag"))
    }
    async fn open_file_url(req: &mut RequestCtx) -> Result<()> {
        let path = req
            .url
            .to_file_path()
            .map_err(|_| anyhow::Error::msg("Can't convert link to file path"))?;
        let file = File::open(&path).await?;
        let lines = BufReader::new(file);
        match path.extension().map(|x| x.to_str()) {
            Some(Some("gmi")) | Some(Some("gemini")) => {
                Self::display_gemini(&mut req.draw_ctx, lines).await?;
            }
            _ => {
                Self::display_text(&mut req.draw_ctx, lines).await?;
            }
        }
        Ok(())
    }
    async fn send_request(req: &mut RequestCtx) -> Result<Option<Vec<u8>>> {
        req.draw_ctx.clear();
        match req.url.scheme() {
            "about" => {
                let reader = futures::io::BufReader::new(common::ABOUT_PAGE.as_bytes());
                Self::display_gemini(&mut req.draw_ctx, reader).await?;
                Ok(None)
            }
            "file" => {
                Self::open_file_url(req).await?;
                Ok(None)
            }
            "gemini" => Self::open_gemini_url(req).await,
            _ => {
                Self::display_url_confirmation(&mut req.draw_ctx, &req.url);
                Ok(None)
            }
        }
    }
    async fn open_gemini_url(req: &mut RequestCtx) -> anyhow::Result<Option<Vec<u8>>> {
        let res: gemini::Response = req.gemini_client.fetch(req.url.as_str()).await?;

        use gemini::Status::*;
        let meta = res.meta().to_owned();
        let status = res.status();
        debug!("Status: {:?}", &status);
        let res = match status {
            Input(_) => {
                Self::display_input(&mut req.draw_ctx, req.url.clone(), &meta);
                None
            }
            Success(_) => {
                let body = res.body().context("Body not found")?;
                let buffered = futures::io::BufReader::new(body);
                if meta.contains("text/gemini") {
                    let res = Self::display_gemini(&mut req.draw_ctx, buffered).await?;
                    Some(res)
                } else if meta.contains("text") {
                    Self::display_text(&mut req.draw_ctx, buffered).await?;
                    None
                } else {
                    Self::display_download(&mut req.draw_ctx, req.url.clone(), buffered).await?;
                    None
                }
            }
            Redirect(_) => bail!("Redirected more than 5 times"),
            TempFail(_) => bail!("Temporary server failure"),
            PermFail(_) => bail!("Permanent server failure"),
            CertRequired(_) => bail!("A certificate is required to access this page"),
        };
        Ok(res)
    }
    fn parse_link(&self, link: &str) -> Result<Url, url::ParseError> {
        let imp = self.imp();
        let current_url = imp
            .history
            .borrow_mut()
            .last()
            .expect("History item not found")
            .url
            .clone();
        let link_url = Url::options().base_url(Some(&current_url)).parse(link)?;
        Ok(link_url)
    }

    fn download_path(url: &Url) -> anyhow::Result<std::path::PathBuf> {
        let file_name = url
            .path_segments()
            .context("Can't divide url in segments")?
            .last()
            .context("Can't get last url segment")?;

        let mut file_name = std::path::PathBuf::from(file_name);
        loop {
            let mut d_path = common::DOWNLOAD_PATH.join(&file_name);
            if d_path.exists() {
                let mut name_no_ext = file_name
                    .file_stem()
                    .context("Can't get file_stem (filename without ext)")?
                    .to_owned();
                let empty_os_string = &std::ffi::OsString::from("");
                let ext = d_path.extension().unwrap_or(empty_os_string);
                file_name = {
                    name_no_ext.push("_new.");
                    name_no_ext.push(ext);
                    std::path::PathBuf::from(name_no_ext)
                };
                d_path.set_file_name(&file_name);
            } else {
                break Ok(d_path);
            }
        }
    }
    async fn display_download<T: AsyncRead + Unpin>(
        ctx: &mut DrawCtx,
        url: Url,
        mut stream: T,
    ) -> anyhow::Result<()> {
        // FIXME: iter moves
        let d_path = Self::download_path(&url)?;

        let mut buffer = Vec::with_capacity(8192);
        buffer.extend_from_slice(&[0; 8192]);

        let mut read = 0;
        let mut text_iter = ctx.text_buffer.end_iter();
        ctx.insert_paragraph(
            &mut text_iter,
            &format!("writing to {:?}\n", d_path.as_os_str()),
        );
        ctx.insert_paragraph(
            &mut text_iter,
            "to interrupt the download, leave this page\n",
        );
        ctx.insert_paragraph(&mut text_iter, "downloaded\t KB\n");

        let mut file = File::create(&d_path).await?;
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    file.write_all(&buffer[..n]).await?;
                    read += n;
                    debug!("lines {}", ctx.text_buffer.line_count());
                    let old_line_iter = ctx
                        .text_buffer
                        .iter_at_line(ctx.text_buffer.line_count() - 2);
                    ctx.text_buffer
                        .delete(&mut old_line_iter.unwrap(), &mut ctx.text_buffer.end_iter());
                    ctx.insert_paragraph(
                        &mut old_line_iter.unwrap(),
                        &format!("downloaded\t {}KB\n", read / 1000),
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        let mut text_iter = ctx.text_buffer.end_iter();
        ctx.insert_paragraph(&mut text_iter, "download finished!\n");
        let downloaded_file_url = format!("file://{}", d_path.as_os_str().to_str().unwrap());
        let button = gtk::Button::with_label("Open With Default Program");
        button.add_css_class("suggested-action");
        button.connect_clicked(move |_| {
            gtk::show_uri(None::<&gtk::Window>, &downloaded_file_url, 0);
        });
        ctx.insert_widget(&mut text_iter, &button);

        Ok(())
    }
    async fn display_text(
        draw_ctx: &mut DrawCtx,
        mut stream: impl AsyncBufRead + Unpin,
    ) -> anyhow::Result<()> {
        let mut line = String::with_capacity(1024);
        loop {
            line.clear();
            let n = stream.read_line_lossy(&mut line).await?;
            if n == 0 {
                break Ok(());
            }
            let text_iter = &mut draw_ctx.text_buffer.end_iter();
            draw_ctx.insert_paragraph(text_iter, &line);
            draw_ctx.insert_paragraph(text_iter, "\n");
        }
    }

    fn display_input(ctx: &mut DrawCtx, url: Url, msg: &str) {
        let text_buffer = &ctx.text_buffer;

        let mut iter = text_buffer.end_iter();
        ctx.insert_paragraph(&mut iter, msg);
        ctx.insert_paragraph(&mut iter, "\n");

        let text_input = gtk::Entry::new();
        text_input.set_hexpand(true);
        text_input.set_width_chars(70);
        text_input.connect_activate(move |text_input| {
            let query = text_input.text().to_string();
            let mut url = url.clone();
            url.set_query(Some(&query));
            text_input
                .activate_action("win.open-omni", Some(&url.to_string().to_variant()))
                .unwrap();
        });
        ctx.insert_widget(&mut iter, &text_input);
    }

    fn display_url_confirmation(ctx: &mut DrawCtx, url: &Url) {
        let mut text_iter = ctx.text_buffer.end_iter();
        ctx.insert_paragraph(
            &mut text_iter,
            "Geopard doesn't support this url scheme. 
If you want to open the following link in an external application, \
click on the button below\n",
        );
        ctx.insert_paragraph(&mut text_iter, &format!("Trying to open: {}\n", url));

        let button = gtk::Button::with_label("Open Externally");
        button.add_css_class("suggested-action");
        let url = url.clone();
        button.connect_clicked(move |_| {
            gtk::show_uri(None::<&gtk::Window>, url.as_str(), 0);
        });
        ctx.insert_widget(&mut text_iter, &button);
        // FIXME: Handle open
    }
    async fn display_gemini<T: AsyncBufRead + Unpin>(
        draw_ctx: &mut DrawCtx,
        mut reader: T,
    ) -> anyhow::Result<Vec<u8>> {
        let mut parser = gemini::Parser::new();
        let mut text_iter = draw_ctx.text_buffer.end_iter();

        let mut preformatted = String::new();
        let mut data = String::with_capacity(1024);
        let mut total = 0;
        let mut n;

        loop {
            n = reader.read_line_lossy(&mut data).await?;
            if n == 0 {
                break;
            }
            let line = &data[total..];
            let token = parser.parse_line(line);
            total += n;
            if let PageElement::Preformatted(line) = token {
                preformatted.push_str(&line);
            } else {
                if !preformatted.is_empty() {
                    draw_ctx.insert_preformatted(&mut text_iter, &preformatted);
                    preformatted.clear();
                }
                match token {
                    PageElement::Text(line) => {
                        draw_ctx.insert_paragraph(&mut text_iter, &line);
                    }
                    PageElement::Heading(line) => {
                        draw_ctx.insert_heading(&mut text_iter, &line);
                    }
                    PageElement::Quote(line) => {
                        draw_ctx.insert_quote(&mut text_iter, &line);
                    }
                    PageElement::Empty => {
                        draw_ctx.insert_paragraph(&mut text_iter, "\n");
                    }
                    PageElement::Link(url, label) => {
                        draw_ctx.insert_link(&mut text_iter, url, label.as_deref());
                    }
                    PageElement::Preformatted(_) => unreachable!("handled before"),
                }
            }
        }
        Ok(data.into_bytes())
    }
}

use std::{collections::HashMap, ffi::OsStr, path::PathBuf, rc::Rc, thread, time::Duration};

use abi_stable::{
    external_types::crossbeam_channel::RSender,
    library::lib_header_from_path,
    std_types::{
        RBoxError,
        RResult::{self, RErr, ROk},
        RString,
    },
};
use anyhow::Result;
use colored::Colorize;
use gtk::{prelude::*, CssProvider, Widget};
use notify::Watcher;
use ron::ser::PrettyConfig;
use tokio::{
    runtime::Handle,
    sync::{mpsc::unbounded_channel, Mutex},
};

use crate::{
    config::{self, Config, GeneralConfig},
    layout_manager::{
        layout_manager_base::LayoutManager,
        simple_layout::SimpleLayout,
    },
};

use dynisland_abi::{ModuleBuilderRef, ModuleType, UIServerCommand};

pub enum BackendServerCommand {
    ReloadConfig(),
}

pub struct App {
    pub application: gtk::Application,
    // pub window: gtk::Window,
    pub module_map: Rc<Mutex<HashMap<String, ModuleType>>>,
    pub layout: Option<Rc<Mutex<Box<dyn LayoutManager>>>>,
    // pub producers_handle: Handle,
    // pub producers_shutdown: tokio::sync::mpsc::Sender<()>,
    pub app_send: Option<RSender<UIServerCommand>>,
    pub config: Config,
    pub css_provider: CssProvider,
}

pub const MODS_DEBUG_PATH: &str =
    "/home/david/dev/rust/dynisland/dynisland-core/target/debug/libexample_module.so";

impl App {
    pub fn initialize_server(mut self) -> Result<()> {
        log::info!("pid: {}", std::process::id());
        //default css

        let (app_send, app_recv) =
            abi_stable::external_types::crossbeam_channel::unbounded::<UIServerCommand>();

        self.app_send = Some(app_send.clone());

        let (app_send_async, mut app_recv_async) = unbounded_channel::<UIServerCommand>();

        thread::spawn(move || {
            while let Ok(msg) = app_recv.recv() {
                app_send_async.send(msg).expect("failed to send message");
            }
        });

        self.layout = Some(Rc::new(Mutex::new(self.load_layout_manager())));
        let module_order = self.load_modules();

        self.load_layout_config();
        self.load_configs();

        self.init_loaded_modules(&module_order);

        // let rt = self.producers_handle.clone();
        // let module_map = self.module_map.clone();

        // let app_send1=self.app_send.clone().unwrap();
        // glib::MainContext::default().spawn_local(async move {
        //     glib::timeout_future_seconds(10).await;
        //     debug!("reloading config");
        //     app_send1.send(UIServerCommand::ReloadConfig()).unwrap();
        // });

        let layout = self.layout.clone().unwrap();
        let widget = layout.blocking_lock().get_primary_widget();
        // let act_container1 = act_container.clone();
        self.application.connect_activate(move |app| {
            let window = gtk::ApplicationWindow::new(app);
            window.set_child(Some(&widget));

            // gtk::Window::set_interactive_debugging(true);

            //show window
            window.connect_destroy(|_| std::process::exit(0));
            window.present();

            // crate::start_fps_counter(&window, log::Level::Trace, Duration::from_millis(200));
        });
        //UI command consumer
        glib::MainContext::default().spawn_local(async move {
            // TODO check if there are too many tasks on the UI thread and it begins to lag
            while let Some(command) = app_recv_async.recv().await {
                match command {
                    // UIServerCommand::AddProducer(module_identifier, producer) => {
                    //     let map_lock = module_map.lock().await;
                    //     let module = map_lock
                    //         .get(&module_identifier)
                    //         .unwrap_or_else(|| panic!("module {} not found", module_identifier));

                    //     // module.register_producer(producer).await; //add inside module
                    //     producer(
                    //         // execute //TODO make sure this doesn't get executed twice at the same time when the configuration is being reloaded
                    //         module.as_ref(),
                    //         &rt,
                    //         app_send.clone(),
                    //     );
                    //     info!("registered producer {}", module_identifier);
                    // }
                    UIServerCommand::AddActivity(activity_identifier, activity) => {
                        let activity: Widget = activity.try_into().unwrap();
                        layout
                            .lock()
                            .await
                            .add_activity(&activity_identifier, activity.clone());

                        Self::update_general_configs_on_activity(
                            &self.config.general_style_config,
                            &activity,
                        );
                        log::info!("registered activity on {}", activity_identifier.module());
                    }
                    UIServerCommand::RemoveActivity(activity_identifier) => {

                        layout.lock().await.remove_activity(&activity_identifier);
                    }
                }
            }
        });

        let (server_send, mut server_recv) = unbounded_channel::<BackendServerCommand>();
        let app = self.application.clone();
        //server command consumer
        glib::MainContext::default().spawn_local(async move {
            let fallback_provider = gtk::CssProvider::new();
            fallback_provider.load_from_string(include_str!("../default.css"));
            gtk::style_context_add_provider_for_display(
                &gdk::Display::default().unwrap(),
                &fallback_provider,
                gtk::STYLE_PROVIDER_PRIORITY_SETTINGS,
            );

            //init css provider
            gtk::style_context_add_provider_for_display(
                &gdk::Display::default().unwrap(),
                &self.css_provider,
                gtk::STYLE_PROVIDER_PRIORITY_USER,
            );
            self.load_css(); //load user's scss
            
            self.restart_producer_rt();
            while let Some(command) = server_recv.recv().await {
                match command {
                    BackendServerCommand::ReloadConfig() => {
                        //FIXME split config and css reload (producers don't need to be restarted if only css changed)

                        // without this sleep, reading the config file sometimes gives an empty file.
                        glib::timeout_future(std::time::Duration::from_millis(50)).await;
                        self.load_configs();
                        self.update_general_configs();
                        self.load_layout_config();
                        self.load_css();

                        self.restart_producer_rt();
                    }
                }
            }
        });
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(evt) => {
                    match evt.kind {
                        notify::EventKind::Create(_) | notify::EventKind::Modify(_) => server_send
                            .send(BackendServerCommand::ReloadConfig())
                            .expect("failed to send notification"),
                        _ => {}
                    }
                    // debug!("{evt:?}");
                }
                Err(err) => {
                    log::error!("notify watcher error: {err}")
                }
            })
            .expect("failed to start file watcher");
        watcher
            .watch(
                &config::get_config_path(),
                notify::RecursiveMode::NonRecursive,
            )
            .expect("error starting watcher");
        //start application
        app.run();
        Ok(())
    }

    pub fn load_css(&mut self) {
        let css_content = grass::from_path(
            config::get_config_path().join("dynisland.scss"),
            &grass::Options::default(),
        );
        match css_content {
            Ok(content) => {
                self.css_provider //TODO save previous state before trying to update
                    .load_from_string(&content);
            }
            Err(err) => {
                log::error!(
                    "{} {:?}",
                    "failed to parse css:".red(),
                    err.to_string().red()
                );
            }
        }
    }

    pub fn load_modules(&mut self) -> Vec<String> {
        //TODO move to config
        self.config = config::get_config();
        let mut module_order = vec![];
        let mut module_def_map = HashMap::<
            String,
            extern "C" fn(RSender<UIServerCommand>) -> RResult<ModuleType, RBoxError>,
        >::new();
        let module_path = {
            #[cfg(debug_assertions)]
            {
                PathBuf::from("/home/david/dev/rust/dynisland/dynisland-core/target/debug/")
            }

            #[cfg(not(debug_assertions))]
            {
                config::get_config_path().join("modules")
            }
        };
        let files = std::fs::read_dir(module_path).unwrap();
        for file in files {
            let file = file.unwrap();
            let path = file.path();
            if !path.is_file() || path.extension().unwrap_or(OsStr::new("")) != "so" {
                continue;
            }
            log::debug!("loading module file: {:#?}", path);

            let res = (|| {
                let header = lib_header_from_path(&path)?;
                header.init_root_module::<ModuleBuilderRef>()
            })();

            let module_builder = match res {
                Ok(x) => x,
                Err(e) => {
                    log::error!(
                        "error while loading {}: {e:#?}",
                        path.file_name().unwrap().to_str().unwrap()
                    );
                    continue;
                }
            };
            let name = module_builder.name();
            let constructor = module_builder.new();

            module_def_map.insert(name.into(), constructor);
        }

        if self.config.loaded_modules.contains(&"all".to_string()) {
            //load all modules available in order of hash (random order)
            for module_def in module_def_map {
                let module_name = module_def.0;
                let module_constructor = module_def.1;

                let built_module = match module_constructor(self.app_send.as_ref().unwrap().clone())
                {
                    ROk(x) => x,
                    RErr(e) => {
                        log::error!("error during creation of {module_name}: {e:#?}");
                        continue;
                    }
                };

                module_order.push(module_name.to_string());
                self.module_map
                    .blocking_lock()
                    .insert(module_name.to_string(), built_module);
            }
        } else {
            //load only modules in the config in order of definition
            for module_name in self.config.loaded_modules.iter() {
                let module_constructor = module_def_map.get(module_name);
                let module_constructor = match module_constructor {
                    None => {
                        log::info!("module {} not found, skipping", module_name);
                        continue;
                    }
                    Some(x) => x,
                };

                let built_module = match module_constructor(self.app_send.as_ref().unwrap().clone())
                {
                    ROk(x) => x,
                    RErr(e) => {
                        log::error!("error during creation of {module_name}: {e:#?}");
                        continue;
                    }
                };
                module_order.push(module_name.to_string());
                // info!("loading module {}", module.get_name());
                self.module_map
                    .blocking_lock()
                    .insert(module_name.to_string(), built_module);
            }
        }

        log::info!("loaded modules: {:?}", module_order);
        module_order
    }

    fn load_configs(&mut self) {
        self.config = config::get_config();
        log::debug!("general_config: {:#?}", self.config.general_style_config);
        for (module_name, module) in self.module_map.blocking_lock().iter_mut() {
            log::info!("loading config for module: {:#?}", module_name);
            let config_to_parse = self.config.module_config.get(module_name);
            let config_parsed = match config_to_parse {
                Some(conf) => {
                    let confs: RString =
                        ron::ser::to_string_pretty(&conf.clone(), PrettyConfig::default())
                            .unwrap()
                            .into();
                    // log::debug!("config for : {}", confs);
                    module.update_config(confs)
                }
                None => {
                    log::debug!("no config for module: {:#?}", module_name);
                    ROk(())
                }
            };
            match config_parsed {
                RErr(err) => {
                    log::error!("failed to parse config for module {}: {err:?}", module_name)
                }
                ROk(()) => {
                    // debug!("{}: {:#?}", module_name, config_to_parse);
                }
            }
        }
    }

    fn update_general_configs(&self) {
        let layout = self.layout.clone().unwrap();
        let layout = layout.blocking_lock();
        let activities = layout.list_activities();
        for activity in activities {
            let activity = layout.get_activity(activity).unwrap();
            Self::update_general_configs_on_activity(&self.config.general_style_config, activity);
        }
    }

    fn update_general_configs_on_activity(config: &GeneralConfig, activity: &Widget) {
        //TODO define property names as constants
        activity.set_property("config-minimal-height-app", config.minimal_height as i32);
        activity.set_property("config-blur-radius-app", config.blur_radius);
    }

    fn init_loaded_modules(&self, order: &Vec<String>) {
        let module_map = self.module_map.blocking_lock();
        for module_name in order {
            let module = module_map.get(module_name).unwrap();
            module.init();
        }
    }

    fn load_layout_manager(&mut self) -> Box<dyn LayoutManager> {
        // let layout_to_load = self
        //     .config
        //     .layout
        //     .clone()
        //     .unwrap_or(simple_layout::NAME.to_string());
        // let mut layout: Option<Box<dyn LayoutManager>> = None;

        // for (layout_name, layout_constructor) in LAYOUTS.iter() {
        //     if layout_name == &layout_to_load {
        //         layout = Some(layout_constructor());
        //         break;
        //     }
        // }
        // let lay = layout.unwrap_or(SimpleLayout::new());

        log::debug!("loading SimpleLayout");
        SimpleLayout::new()
    }

    fn load_layout_config(&self) {
        let layout = self.layout.clone().unwrap();
        let mut layout = layout.blocking_lock();
        let layout_name = layout.get_name();
        if let Some(config) = self.config.layout_configs.get(layout_name) {
            match layout.parse_config(config.clone()) {
                Ok(()) => {
                    log::info!("loaded layout config for {layout_name}");
                }
                Err(err) => {
                    log::error!("failed to parse layout config for {layout_name}, {err}");
                }
            }
        } else {
            log::info!("no layout config found for {layout_name}, using Default");
        }
    }

    fn restart_producer_rt(&self) {
        for module in self.module_map.blocking_lock().values_mut() {
            module.restart_producers();
        }
    }
}

impl Default for App {
    fn default() -> Self {
        // let (hdl, shutdown) = get_new_tokio_rt();
        let app =
            gtk::Application::new(Some("com.github.cr3eperall.dynisland"), Default::default());
        App {
            application: app,
            module_map: Rc::new(Mutex::new(HashMap::new())),
            layout: None,
            // producers_handle: hdl,
            // producers_shutdown: shutdown,
            app_send: None,
            config: config::Config::default(),
            css_provider: gtk::CssProvider::new(),
        }
    }
}

pub fn get_window() -> gtk::Window {
    gtk::Window::builder()
        .title("test")
        .height_request(500)
        .width_request(800)
        .build()
    // gtk_layer_shell::init_for_window(&window);
    // gtk_layer_shell::set_layer(&window, gtk_layer_shell::Layer::Overlay);
    // gtk_layer_shell::set_anchor(&window, gtk_layer_shell::Edge::Top, true);
    // gtk_layer_shell::set_margin(&window, gtk_layer_shell::Edge::Top, 5);

    // window
}

pub fn get_new_tokio_rt() -> (Handle, tokio::sync::mpsc::Sender<()>) {
    let (rt_send, rt_recv) =
        tokio::sync::oneshot::channel::<(Handle, tokio::sync::mpsc::Sender<()>)>();
    let (shutdown_send, mut shutdown_recv) = tokio::sync::mpsc::channel::<()>(1);
    std::thread::Builder::new()
        .name("dyn-producers".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("idk tokio rt failed");
            let handle = rt.handle();
            rt_send
                .send((handle.clone(), shutdown_send))
                .expect("failed to send rt");
            rt.block_on(async { shutdown_recv.recv().await }); //keep thread alive
        })
        .expect("failed to spawn new trhread");

    rt_recv.blocking_recv().expect("failed to receive rt")
}

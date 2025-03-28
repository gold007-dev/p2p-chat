use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::stream::StreamExt;
use libp2p::{
    gossipsub, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId,
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
    time::Duration,
};
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    text::Text,
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use tokio::{sync::mpsc, task};

#[derive(NetworkBehaviour)]
struct ChatBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

struct AppState {
    messages: Vec<String>,
    input: String,
    peer_id: String,
}

impl AppState {
    fn new(peer_id: PeerId) -> Self {
        Self {
            messages: vec![format!("Peer ID: {}", peer_id)],
            input: String::new(),
            peer_id: peer_id.to_string(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Channels für die Kommunikation zwischen Tasks
    let (ui_tx, mut ui_rx) = mpsc::channel(100);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(100);
    
    let state = Arc::new(Mutex::new(AppState::new(
        libp2p::identity::Keypair::generate_ed25519().public().to_peer_id()
    )));

    // Terminal Setup
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // libp2p Setup
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|key| {
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(((s.finish() as u128)*(rand::random::<u8>() as u128)).to_string())
            };

            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .message_id_fn(message_id_fn)
                .build()
                .unwrap();

            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;

            let mdns = mdns::tokio::Behaviour::new(
                mdns::Config::default(),
                key.public().to_peer_id(),
            )?;
            Ok(ChatBehaviour { gossipsub, mdns })
        })?
        .build();

    let topic = gossipsub::IdentTopic::new("p2p-chat");
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    // Netzwerk-Task
    let network_state = state.clone();
    let topic_clone = topic.clone();
    task::spawn(async move {
        let mut swarm = swarm;
        loop {
            tokio::select! {
                event = swarm.select_next_some() => match event {
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        for (peer_id, _) in list {
                            swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                        }
                    },
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        for (peer_id, _) in list {
                            swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                        }
                    },
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        propagation_source: peer_id,
                        message,
                        ..
                    })) => {
                        let msg = format!("{}: {}", peer_id, String::from_utf8_lossy(&message.data));
                        ui_tx.send(msg).await.unwrap();
                    },
                    SwarmEvent::NewListenAddr { address, .. } => {
                        let mut s = network_state.lock().unwrap();
                        s.messages.push(format!("Listening on: {}", address));
                    },
                    _ => {}
                },
                cmd = cmd_rx.recv() => {
                    if let Some(msg) = cmd {
                        swarm.behaviour_mut().gossipsub
                            .publish(topic_clone.clone(), msg.as_bytes())
                            .unwrap();
                    }
                }
            }
        }
    });

    // Main UI loop
    loop {
        terminal.draw(|f| ui(f, &state.lock().unwrap()))?;

        // Event handling
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let mut s = state.lock().unwrap();
                    match key.code {
                        KeyCode::Enter => {
                            if !s.input.is_empty() {
                                let msg = s.input.clone();
                                let peer_id = s.peer_id.clone();
                                cmd_tx.send(msg.clone()).await.unwrap();
                                s.messages.push(format!("{} (You): {}", peer_id, msg));
                                s.input.clear();
                            }
                        }
                        KeyCode::Char(c) => s.input.push(c),
                        KeyCode::Backspace => { s.input.pop(); }
                        KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }
        }

        // Update messages
        while let Ok(msg) = ui_rx.try_recv() {
            let mut s = state.lock().unwrap();
            s.messages.push(msg);
        }
    }

    // Cleanup
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

fn ui<B: Backend>(f: &mut Frame<B>, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(3), Constraint::Length(3)].as_slice())
        .split(f.size());

    let messages: Vec<ListItem> = state
        .messages
        .iter()
        .rev()
        .map(|m| ListItem::new(m.as_str()))
        .collect();

    let list = List::new(messages)
        .block(Block::default().title("Chat").borders(Borders::ALL));
    
    let input = Paragraph::new(Text::from(state.input.as_str()))
        .block(Block::default().title("Input").borders(Borders::ALL));

    f.render_widget(list, chunks[0]);
    f.render_widget(input, chunks[1]);
}

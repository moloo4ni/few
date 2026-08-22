use ratatui::style::{Modifier, Style};

pub fn normal() -> Style {
    Style::new()
}

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn red() -> Style {
    Style::new().fg(ratatui::style::Color::Red)
}

pub fn green() -> Style {
    Style::new().fg(ratatui::style::Color::Green)
}

pub fn amber() -> Style {
    Style::new().fg(ratatui::style::Color::Yellow)
}

pub fn blue() -> Style {
    Style::new().fg(ratatui::style::Color::Blue)
}

pub fn blue_dim() -> Style {
    Style::new()
        .fg(ratatui::style::Color::Blue)
        .add_modifier(Modifier::DIM)
}

pub fn reversed() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

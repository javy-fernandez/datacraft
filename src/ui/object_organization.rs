//! Local object folders and colors; never changes database objects.
use crate::i18n::tr;
use egui::{Color32, RichText, Ui};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Organization {
    pub conn: u64,
    pub db: String,
    pub groups: Vec<String>,
    group_colors: BTreeMap<String, [u8; 3]>,
    objects: Vec<ObjectStyle>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct ObjectStyle {
    schema: String,
    table: String,
    group: Option<String>,
    color: Option<[u8; 3]>,
}

impl Organization {
    fn style(&self, schema: &str, table: &str) -> Option<&ObjectStyle> {
        self.objects
            .iter()
            .find(|o| o.schema == schema && o.table == table)
    }

    fn style_mut(&mut self, schema: &str, table: &str) -> &mut ObjectStyle {
        let index = self
            .objects
            .iter()
            .position(|o| o.schema == schema && o.table == table)
            .unwrap_or_else(|| {
                self.objects.push(ObjectStyle {
                    schema: schema.into(),
                    table: table.into(),
                    ..Default::default()
                });
                self.objects.len() - 1
            });
        &mut self.objects[index]
    }

    /// Nothing to remember (not persisted).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
            && self.group_colors.is_empty()
            && self
                .objects
                .iter()
                .all(|o| o.group.is_none() && o.color.is_none())
    }

    /// The table was dropped (`new_name` None) or renamed: its folder and color go with it, so a
    /// new table with the old name doesn't inherit them.
    pub fn table_changed(&mut self, schema: &str, table: &str, new_name: Option<&str>) {
        match new_name {
            Some(new_name) => {
                self.objects
                    .retain(|o| !(o.schema == schema && o.table == new_name));
                if let Some(o) = self
                    .objects
                    .iter_mut()
                    .find(|o| o.schema == schema && o.table == table)
                {
                    o.table = new_name.into();
                }
            }
            None => self
                .objects
                .retain(|o| !(o.schema == schema && o.table == table)),
        }
    }

    pub fn color(&self, schema: &str, table: &str) -> Option<Color32> {
        self.style(schema, table)
            .and_then(|o| o.color)
            .map(|[r, g, b]| Color32::from_rgb(r, g, b))
    }

    pub fn group(&self, schema: &str, table: &str) -> Option<&str> {
        self.style(schema, table).and_then(|o| o.group.as_deref())
    }

    pub fn group_color(&self, name: &str) -> Option<Color32> {
        self.group_colors
            .get(name)
            .map(|&[r, g, b]| Color32::from_rgb(r, g, b))
    }

    pub fn assign(
        &mut self,
        tables: &[crate::ui::explorer::TableRef],
        group: Option<&str>,
    ) -> bool {
        if group.is_some_and(|name| !self.groups.iter().any(|g| g == name))
            || tables
                .iter()
                .any(|t| t.conn != self.conn || t.db != self.db)
        {
            return false;
        }
        for table in tables {
            self.style_mut(&table.schema, &table.table).group = group.map(str::to_string);
        }
        true
    }

    pub fn create(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.groups.iter().any(|g| g == name) {
            return false;
        }
        self.groups.push(name.into());
        true
    }

    pub fn rename(&mut self, old: &str, name: &str) -> bool {
        if name.trim().is_empty()
            || self.groups.iter().any(|g| g == name)
            || !self.groups.iter().any(|g| g == old)
        {
            return false;
        }
        for group in &mut self.groups {
            if group == old {
                *group = name.into();
            }
        }
        for style in &mut self.objects {
            if style.group.as_deref() == Some(old) {
                style.group = Some(name.into());
            }
        }
        if let Some(color) = self.group_colors.remove(old) {
            self.group_colors.insert(name.into(), color);
        }
        true
    }

    pub fn remove(&mut self, name: &str) {
        self.groups.retain(|g| g != name);
        self.group_colors.remove(name);
        for style in &mut self.objects {
            if style.group.as_deref() == Some(name) {
                style.group = None;
            }
        }
    }

    pub fn menu(
        &mut self,
        ui: &mut Ui,
        tables: &[crate::ui::explorer::TableRef],
        groups: &[String],
    ) -> bool {
        let mut create = false;
        ui.menu_button(tr("Add to group…"), |ui| {
            if ui.button(tr("Ungrouped")).clicked() {
                self.assign(tables, None);
                ui.close();
            }
            if ui.button(tr("New group…")).clicked() {
                create = true;
                ui.close();
            }
            ui.separator();
            let mut chosen = None;
            for group in &self.groups {
                if ui.button(group).clicked() {
                    chosen = Some(group.clone());
                    ui.close();
                }
            }
            if let Some(group) = chosen {
                self.assign(tables, Some(&group));
            }
        });
        self.color_menu(ui, tables, groups);
        create
    }

    pub fn color_menu(
        &mut self,
        ui: &mut Ui,
        tables: &[crate::ui::explorer::TableRef],
        groups: &[String],
    ) {
        if let Some(color) = color_menu(ui) {
            for table in tables {
                self.style_mut(&table.schema, &table.table).color = color;
            }
            for group in groups.iter().filter(|g| self.groups.contains(g)) {
                match color {
                    Some(color) => {
                        self.group_colors.insert(group.clone(), color);
                    }
                    None => {
                        self.group_colors.remove(group);
                    }
                }
            }
        }
    }
}

// Outer None means no action; Some(None) resets the selected objects to their default.
fn color_menu(ui: &mut Ui) -> Option<Option<[u8; 3]>> {
    let mut chosen = None;
    ui.menu_button(tr("Color"), |ui| {
        if ui.button(tr("Default")).clicked() {
            chosen = Some(None);
            ui.close();
        }
        for (name, color) in [
            (tr("Red"), [210, 85, 90]),
            (tr("Orange"), [201, 133, 45]),
            (tr("Green"), [65, 163, 112]),
            (tr("Blue"), [81, 143, 221]),
            (tr("Purple"), [161, 116, 212]),
            (tr("Pink"), [204, 110, 165]),
        ] {
            if ui
                .button(RichText::new(name).color(Color32::from_rgb(color[0], color[1], color[2])))
                .clicked()
            {
                chosen = Some(Some(color));
                ui.close();
            }
        }
    });
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]

    fn groups_and_colors_roundtrip_without_touching_other_schemas() {
        let mut org = Organization {
            conn: 7,
            db: "shop".into(),
            groups: vec!["Sales".into()],
            ..Default::default()
        };
        let style = org.style_mut("public", "orders");
        style.group = Some("Sales".into());
        style.color = Some([81, 143, 221]);
        org.group_colors.insert("Sales".into(), [161, 116, 212]);
        org.rename("Sales", "Orders");
        let mut org: Organization =
            serde_json::from_str(&serde_json::to_string(&org).unwrap()).unwrap();
        assert_eq!(org.group("public", "orders"), Some("Orders"));
        assert_eq!(
            org.group_color("Orders"),
            Some(Color32::from_rgb(161, 116, 212))
        );
        assert!(org.group_color("Sales").is_none());
        assert!(org.group("archive", "orders").is_none());
        assert!(org.color("public", "orders").is_some());
        org.remove("Orders");
        assert!(org.group_color("Orders").is_none());
        assert!(org.group("public", "orders").is_none());
        assert!(org.color("public", "orders").is_some());
    }

    #[test]
    fn dropped_and_renamed_tables_take_their_style_with_them() {
        let mut org = Organization::default();
        org.style_mut("public", "orders").color = Some([1, 2, 3]);
        org.style_mut("public", "items").group = Some("Sales".into());
        org.table_changed("public", "orders", None);
        assert_eq!(
            org.color("public", "orders"),
            None,
            "a new table with the old name starts clean"
        );
        org.table_changed("public", "items", Some("order_items"));
        assert_eq!(org.group("public", "order_items"), Some("Sales"));
        assert_eq!(org.group("public", "items"), None);
        org.table_changed("public", "order_items", None);
        assert!(org.is_empty());
    }
}

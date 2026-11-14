use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const API_BASE: &str =
    "https://codeweek.blathy.info/api/task/HkcFuUnk8AEOjH0qSvZdTfhgVZWgHca5/expedition";

#[derive(Debug)]
struct Source {
    budget: u64,
    weight_limit: u64,
    shop: Url,
    warehouse: Url,
    catalog: Url,
    rates: Url,
}

#[derive(Debug)]
struct ShopItem {
    id: u64,
    sku: String,
    amount: f64,
    currency: String,
    points: i64,
}

#[derive(Debug)]
struct WarehouseItem {
    weight: u64,
    stock: u64,
}

#[derive(Debug)]
struct Catalog {
    required: Vec<String>,
    items: HashMap<u64, Vec<String>>,
}

#[derive(Debug)]
struct Rates {
    version: String,
    huf_per_unit: HashMap<String, f64>,
}

#[derive(Debug, Clone)]
struct Candidate {
    sku: String,
    price: u64,
    weight: u64,
    points: i64,
    categories: u64,
}

#[derive(Debug, Clone)]
struct State {
    cost: u64,
    weight: u64,
    points: i64,
    selected: Vec<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Solution {
    items: Vec<String>,
    rates_version: String,
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let source = get_source(client).await?;
    let (shop, warehouse, catalog) = tokio::try_join!(
        get_shop(client, &source.shop),
        get_warehouse(client, &source.warehouse),
        get_catalog(client, &source.catalog),
    )?;
    let rates = get_rates(client, &source.rates).await?;

    let mut required = catalog
        .required
        .iter()
        .map(|category| normalize(category))
        .collect::<Vec<_>>();
    required.sort();
    required.dedup();
    if required.is_empty() || required.len() > 63 {
        bail!("Catalog must contain between 1 and 63 required categories");
    }
    let required_mask = (1_u64 << required.len()) - 1;
    let category_bits = required
        .iter()
        .enumerate()
        .map(|(index, category)| (category.clone(), 1_u64 << index))
        .collect::<HashMap<_, _>>();

    let mut candidates = Vec::new();
    let mut shop_ids = HashSet::new();
    for product in shop {
        if !shop_ids.insert(product.id) {
            bail!("Shop contains duplicate normalized SKU {}", product.id);
        }
        let (Some(stock), Some(categories)) =
            (warehouse.get(&product.id), catalog.items.get(&product.id))
        else {
            continue;
        };
        if stock.stock == 0 {
            continue;
        }
        let rate = rates
            .huf_per_unit
            .get(&product.currency.to_ascii_uppercase())
            .with_context(|| format!("No HUF exchange rate for {}", product.currency))?;
        let category_mask = categories.iter().fold(0, |mask, category| {
            mask | category_bits
                .get(&normalize(category))
                .copied()
                .unwrap_or(0)
        });
        candidates.push(Candidate {
            sku: product.sku,
            price: round_half_up(product.amount * rate)?,
            weight: stock.weight,
            points: product.points,
            categories: category_mask,
        });
    }

    let selected = optimize(
        &candidates,
        source.budget,
        source.weight_limit,
        required_mask,
    )
    .context("No valid equipment set covers all required categories")?;
    let solution = Solution {
        items: selected
            .iter()
            .map(|index| candidates[*index].sku.clone())
            .collect(),
        rates_version: rates.version,
    };
    let response = client
        .post(format!("{API_BASE}/solution"))
        .json(&solution)
        .send()
        .await
        .context("POST /solution request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read POST /solution response")?;
    if !status.is_success() {
        bail!("POST /solution returned {status}: {body}");
    }
    println!("{body}");
    Ok(())
}

async fn get_source(client: &reqwest::Client) -> Result<Source> {
    let url = Url::parse(&format!("{API_BASE}/source"))?;
    let value = get_json(client, &url).await?;
    let sources = value
        .get("sources")
        .and_then(Value::as_object)
        .context("GET /source has no sources object")?;
    let source_url = |name: &str| -> Result<Url> {
        let text = sources
            .get(name)
            .and_then(Value::as_str)
            .with_context(|| format!("GET /source has no {name} URL"))?;
        Url::parse(text).with_context(|| format!("GET /source has an invalid {name} URL"))
    };
    Ok(Source {
        budget: value
            .get("budgetHuf")
            .and_then(Value::as_u64)
            .context("GET /source has no valid budgetHuf")?,
        weight_limit: value
            .get("weightLimitGrams")
            .and_then(Value::as_u64)
            .context("GET /source has no valid weightLimitGrams")?,
        shop: source_url("shop")?,
        warehouse: source_url("warehouse")?,
        catalog: source_url("catalog")?,
        rates: source_url("rates")?,
    })
}

async fn get_json(client: &reqwest::Client, url: &Url) -> Result<Value> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {url} response"))?;
    if !status.is_success() {
        bail!("GET {url} returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("GET {url} returned invalid JSON"))
}

async fn get_shop(client: &reqwest::Client, first_url: &Url) -> Result<Vec<ShopItem>> {
    let mut next = Some(first_url.clone());
    let mut visited = HashSet::new();
    let mut products = Vec::new();
    while let Some(url) = next {
        if !visited.insert(url.to_string()) {
            bail!("Shop pagination contains a URL cycle at {url}");
        }
        let page = get_json(client, &url).await?;
        let rows = find_array(&page, &["items", "products", "results"])
            .context("Shop page has no product array")?;
        for row in rows {
            let sku = string_field(row, &["sku", "itemCode", "productCode", "code"])
                .context("Shop product has no SKU")?;
            let price = row.get("price").context("Shop product has no price")?;
            products.push(ShopItem {
                id: sku_id(&sku)?,
                sku,
                amount: number_field(price, &["amount", "value"])
                    .context("Shop product has no valid price amount")?,
                currency: string_field(price, &["currency"])
                    .context("Shop product has no price currency")?,
                points: integer_field(row, &["points", "score"])
                    .context("Shop product has no valid integer points")?,
            });
        }
        next = find_field(&page, &["next"])
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(|url| {
                first_url
                    .join(url)
                    .with_context(|| format!("Invalid shop pagination URL: {url}"))
            })
            .transpose()?;
    }
    Ok(products)
}

async fn get_warehouse(client: &reqwest::Client, url: &Url) -> Result<HashMap<u64, WarehouseItem>> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {url} response"))?;
    if !status.is_success() {
        bail!("GET {url} returned {status}: {body}");
    }

    parse_warehouse(&body).with_context(|| format!("GET {url} returned invalid warehouse CSV"))
}

fn parse_warehouse(csv_text: &str) -> Result<HashMap<u64, WarehouseItem>> {
    let first_line = csv_text.lines().next().unwrap_or_default();
    let delimiter = if first_line.matches(';').count() > first_line.matches(',').count() {
        b';'
    } else {
        b','
    };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .from_reader(csv_text.as_bytes());
    let headers = reader
        .headers()
        .context("Warehouse CSV has no header")?
        .clone();
    let sku_column = csv_column(
        &headers,
        &[
            "sku",
            "itemcode",
            "productcode",
            "code",
            "cikkszam",
            "article",
            "articlenumber",
            "articleid",
            "productid",
            "itemid",
            "itemno",
            "partnumber",
            "itemnumber",
        ],
    )?;
    let weight_column = csv_column(
        &headers,
        &["weight", "mass", "suly", "weightgrams", "massgrams"],
    )?;
    let stock_column = csv_column(
        &headers,
        &[
            "stock",
            "quantity",
            "available",
            "keszlet",
            "qty",
            "inventory",
            "stockcount",
        ],
    )?;
    let unit_column = headers.iter().position(|header| {
        matches!(
            normalize(header.trim().trim_start_matches('\u{feff}')).as_str(),
            "unit" | "weightunit" | "sulyegyseg" | "mertekegyseg" | "massunit"
        )
    });

    let mut inventory = HashMap::new();
    for (row_index, row) in reader.records().enumerate() {
        let row = row.with_context(|| format!("Invalid warehouse CSV row {}", row_index + 2))?;
        let sku = row
            .get(sku_column)
            .context("Warehouse CSV row has no SKU")?;
        let raw_weight = row
            .get(weight_column)
            .context("Warehouse CSV row has no weight")?;
        let weight_text = unit_column
            .and_then(|index| row.get(index))
            .filter(|unit| !unit.trim().is_empty())
            .map(|unit| format!("{raw_weight} {unit}"))
            .unwrap_or_else(|| raw_weight.to_owned());
        let (amount, unit) = parse_weight_text(&weight_text)
            .with_context(|| format!("Invalid weight for warehouse SKU {sku}"))?;
        let grams = match unit.trim().to_ascii_lowercase().as_str() {
            "g" => amount,
            "kg" => amount * 1000.0,
            "lb" => amount * 453.59237,
            other => bail!("Unsupported warehouse weight unit {other:?} for SKU {sku}"),
        };
        let weight = round_half_up(grams)?;
        let stock = row
            .get(stock_column)
            .context("Warehouse CSV row has no stock")?
            .trim()
            .parse::<u64>()
            .with_context(|| format!("Invalid stock for warehouse SKU {sku}"))?;
        if inventory
            .insert(sku_id(sku)?, WarehouseItem { weight, stock })
            .is_some()
        {
            bail!("Warehouse CSV contains duplicate normalized SKU {sku}");
        }
    }
    Ok(inventory)
}

fn csv_column(headers: &csv::StringRecord, names: &[&str]) -> Result<usize> {
    headers
        .iter()
        .position(|header| {
            let header = normalize(header.trim().trim_start_matches('\u{feff}'));
            names.iter().any(|name| header == *name)
        })
        .with_context(|| {
            format!(
                "Warehouse CSV is missing the {} column (found: {})",
                names.join("/"),
                headers.iter().collect::<Vec<_>>().join(", ")
            )
        })
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CatalogXml {
    #[serde(default)]
    required_categories: Option<XmlCategories>,
    #[serde(default)]
    required: Option<XmlCategories>,
    #[serde(default)]
    categories: Option<XmlCategories>,
    #[serde(default)]
    items: Option<XmlItems>,
    #[serde(default)]
    products: Option<XmlItems>,
    #[serde(default)]
    category: Vec<XmlCategory>,
    #[serde(default)]
    item: Vec<XmlItem>,
    #[serde(default)]
    product: Vec<XmlItem>,
    #[serde(default)]
    entry: Vec<XmlItem>,
}

#[derive(Debug, Deserialize, Default)]
struct XmlCategories {
    #[serde(default)]
    category: Vec<XmlCategory>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct XmlCategory {
    #[serde(rename = "#text", default)]
    text: String,
    #[serde(rename = "@id", default)]
    id: Option<String>,
    #[serde(rename = "@code", default)]
    code: Option<String>,
    #[serde(rename = "@name", default)]
    name: Option<String>,
    #[serde(rename = "@required", default)]
    required: Option<String>,
    #[serde(rename = "@mandatory", default)]
    mandatory: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct XmlItems {
    #[serde(default)]
    item: Vec<XmlItem>,
    #[serde(default)]
    product: Vec<XmlItem>,
    #[serde(default)]
    entry: Vec<XmlItem>,
}

#[derive(Debug, Deserialize, Default)]
struct XmlItem {
    #[serde(rename = "@sku", default)]
    sku_attribute: Option<String>,
    #[serde(rename = "@code", default)]
    code_attribute: Option<String>,
    #[serde(rename = "@category", default)]
    category_attribute: Option<String>,
    #[serde(rename = "@categories", default)]
    categories_attribute: Option<String>,
    #[serde(default)]
    sku: Option<String>,
    #[serde(rename = "itemCode", default)]
    item_code: Option<String>,
    #[serde(rename = "productCode", default)]
    product_code: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    categories: Option<XmlCategories>,
    #[serde(default)]
    category: Vec<XmlCategory>,
}

impl XmlItems {
    fn into_vec(self) -> Vec<XmlItem> {
        self.item
            .into_iter()
            .chain(self.product)
            .chain(self.entry)
            .collect()
    }
}

async fn get_catalog(client: &reqwest::Client, url: &Url) -> Result<Catalog> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {url} response"))?;
    if !status.is_success() {
        bail!("GET {url} returned {status}: {body}");
    }
    parse_catalog(&body).with_context(|| format!("GET {url} returned invalid catalog XML"))
}

fn parse_catalog(xml: &str) -> Result<Catalog> {
    match parse_catalog_serde(xml) {
        Ok(catalog) if !catalog.items.is_empty() => Ok(catalog),
        serde_result => parse_catalog_events(xml).with_context(|| match serde_result {
            Ok(_) => "serde-xml-rs found no catalog products".to_owned(),
            Err(error) => format!("serde-xml-rs could not parse catalog structure: {error:#}"),
        }),
    }
}

fn parse_catalog_serde(xml: &str) -> Result<Catalog> {
    let document: CatalogXml =
        serde_xml_rs::from_str(xml).context("Catalog XML could not be deserialized")?;
    let mut required_rows = document
        .required_categories
        .or(document.required)
        .map(|categories| categories.category)
        .unwrap_or_default();
    let mut all_categories = document
        .categories
        .map(|categories| categories.category)
        .unwrap_or_default();
    all_categories.extend(document.category.iter().cloned());
    if required_rows.is_empty() {
        required_rows.extend(
            all_categories
                .iter()
                .filter(|category| {
                    xml_truthy(
                        category
                            .required
                            .as_deref()
                            .or(category.mandatory.as_deref()),
                    )
                })
                .cloned(),
        );
    }
    all_categories.extend(required_rows.iter().cloned());
    let mut aliases = HashMap::new();
    for category in &all_categories {
        let name = xml_category_name(category);
        for alias in [
            category.id.as_deref(),
            category.code.as_deref(),
            category.name.as_deref(),
            Some(name.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            aliases.insert(normalize(alias), name.clone());
        }
    }
    let mut required = required_rows
        .iter()
        .map(xml_category_name)
        .collect::<Vec<_>>();
    let mut rows = document.items.map(XmlItems::into_vec).unwrap_or_default();
    if let Some(products) = document.products {
        rows.extend(products.into_vec());
    }
    rows.extend(document.item);
    rows.extend(document.product);
    rows.extend(document.entry);
    let mut items = HashMap::new();
    for row in rows {
        let sku = row
            .sku_attribute
            .or(row.code_attribute)
            .or(row.sku)
            .or(row.item_code)
            .or(row.product_code)
            .or(row.code)
            .context("Catalog item has no SKU")?;
        let mut categories = row
            .categories
            .map(|value| value.category)
            .unwrap_or_default();
        categories.extend(row.category);
        let mut item_categories: Vec<String> = categories
            .iter()
            .map(|category| {
                let key = category
                    .id
                    .as_deref()
                    .or(category.code.as_deref())
                    .map(normalize)
                    .unwrap_or_else(|| xml_category_name(category));
                aliases.get(&key).cloned().unwrap_or(key)
            })
            .collect();
        for value in [row.category_attribute, row.categories_attribute]
            .into_iter()
            .flatten()
        {
            item_categories.extend(value.split([',', ';', '|']).map(|category| {
                let key = normalize(category);
                aliases.get(&key).cloned().unwrap_or(key)
            }));
        }
        if items.insert(sku_id(&sku)?, item_categories).is_some() {
            bail!("Catalog XML contains duplicate normalized SKU {sku}");
        }
    }
    required.sort();
    required.dedup();
    if required.is_empty() {
        bail!("Catalog XML contains no required categories");
    }
    if items.is_empty() {
        bail!("Catalog XML contains no items with SKU values");
    }
    Ok(Catalog { required, items })
}

#[derive(Debug)]
struct XmlNode {
    name: String,
    attributes: HashMap<String, String>,
    text: String,
    children: Vec<XmlNode>,
}

fn parse_catalog_events(xml: &str) -> Result<Catalog> {
    let parser = xml::reader::EventReader::new(xml.as_bytes());
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root = None;
    for event in parser {
        match event.context("Catalog XML parser failed")? {
            xml::reader::XmlEvent::StartElement {
                name, attributes, ..
            } => stack.push(XmlNode {
                name: name.local_name.to_ascii_lowercase(),
                attributes: attributes
                    .into_iter()
                    .map(|attribute| {
                        (
                            attribute.name.local_name.to_ascii_lowercase(),
                            attribute.value,
                        )
                    })
                    .collect(),
                text: String::new(),
                children: Vec::new(),
            }),
            xml::reader::XmlEvent::Characters(text) | xml::reader::XmlEvent::CData(text) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&text);
                }
            }
            xml::reader::XmlEvent::EndElement { .. } => {
                let node = stack
                    .pop()
                    .context("Catalog XML has unmatched closing tag")?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    root = Some(node);
                }
            }
            _ => {}
        }
    }
    let root = root.context("Catalog XML has no root element")?;
    catalog_from_xml_tree(&root)
}

fn catalog_from_xml_tree(root: &XmlNode) -> Result<Catalog> {
    let mut all_nodes = Vec::new();
    collect_xml_nodes(root, &mut all_nodes);
    let mut aliases = HashMap::new();
    let mut required = Vec::new();
    for node in &all_nodes {
        if !is_category_name(&node.name) || has_item_ancestor(root, node) {
            continue;
        }
        let value = xml_node_category_value(node);
        if value.is_empty() {
            continue;
        }
        let canonical = normalize(&value);
        for alias in [
            node.attributes.get("id"),
            node.attributes.get("code"),
            node.attributes.get("name"),
            Some(&value),
        ]
        .into_iter()
        .flatten()
        {
            aliases.insert(normalize(alias), canonical.clone());
        }
        if node.attributes.iter().any(|(name, value)| {
            matches!(name.as_str(), "required" | "mandatory") && xml_truthy(Some(value))
        }) || has_required_ancestor(root, node)
        {
            required.push(canonical);
        }
    }
    required.sort();
    required.dedup();
    if required.is_empty() {
        bail!("Catalog XML contains no required categories");
    }

    let mut item_nodes = Vec::new();
    for node in &all_nodes {
        if is_item_node(node) && !has_item_ancestor(root, node) {
            item_nodes.push(*node);
        }
    }
    let mut items = HashMap::new();
    for node in item_nodes {
        let sku = xml_node_sku(node).with_context(|| {
            format!("Catalog XML item element <{}> has no SKU field", node.name)
        })?;
        let mut categories = Vec::new();
        collect_item_categories(node, &mut categories);
        let categories = categories
            .into_iter()
            .map(|value| {
                let key = normalize(&value);
                aliases.get(&key).cloned().unwrap_or(key)
            })
            .collect();
        if items.insert(sku_id(&sku)?, categories).is_some() {
            bail!("Catalog XML contains duplicate normalized SKU {sku}");
        }
    }
    if items.is_empty() {
        bail!("Catalog XML contains no items with SKU values");
    }
    Ok(Catalog { required, items })
}

fn collect_xml_nodes<'a>(node: &'a XmlNode, output: &mut Vec<&'a XmlNode>) {
    output.push(node);
    for child in &node.children {
        collect_xml_nodes(child, output);
    }
}

fn has_item_ancestor(root: &XmlNode, target: &XmlNode) -> bool {
    fn find(node: &XmlNode, target: *const XmlNode, already_item: bool) -> Option<bool> {
        let is_item = already_item || is_item_node(node);
        if std::ptr::eq(node, target) {
            return Some(already_item);
        }
        for child in &node.children {
            if let Some(has_item) = find(child, target, is_item) {
                return Some(has_item);
            }
        }
        None
    }
    find(root, target, false).unwrap_or(false)
}

fn has_required_ancestor(root: &XmlNode, target: &XmlNode) -> bool {
    fn find(node: &XmlNode, target: *const XmlNode, required: bool) -> Option<bool> {
        let required =
            required || node.name.contains("required") || node.name.contains("mandatory");
        if std::ptr::eq(node, target) {
            return Some(
                required && !node.name.contains("required") && !node.name.contains("mandatory"),
            );
        }
        for child in &node.children {
            if let Some(value) = find(child, target, required) {
                return Some(value);
            }
        }
        None
    }
    find(root, target, false).unwrap_or(false)
}

fn is_category_name(name: &str) -> bool {
    name.contains("categor")
}

fn is_item_node(node: &XmlNode) -> bool {
    has_sku_field(node)
        || (matches!(
            normalize(&node.name).as_str(),
            "item" | "product" | "entry" | "gear" | "equipment"
        ) && node.attributes.contains_key("id"))
}

fn has_sku_field(node: &XmlNode) -> bool {
    [
        "sku",
        "itemcode",
        "productcode",
        "article",
        "articlenumber",
        "productid",
        "itemid",
        "partnumber",
        "itemnumber",
        "cikkszam",
        "code",
    ]
    .iter()
    .any(|field| node.attributes.contains_key(*field))
        || node.children.iter().any(|child| {
            [
                "sku",
                "itemcode",
                "productcode",
                "article",
                "productid",
                "itemid",
                "partnumber",
                "itemnumber",
                "cikkszam",
                "code",
            ]
            .contains(&normalize(&child.name).as_str())
                && !child.text.trim().is_empty()
        })
        || (matches!(
            normalize(&node.name).as_str(),
            "item" | "product" | "entry" | "gear" | "equipment"
        ) && node
            .children
            .iter()
            .any(|child| normalize(&child.name) == "id" && !child.text.trim().is_empty()))
}

fn xml_node_sku(node: &XmlNode) -> Option<String> {
    for name in [
        "sku",
        "itemcode",
        "productcode",
        "article",
        "articlenumber",
        "productid",
        "itemid",
        "partnumber",
        "itemnumber",
        "cikkszam",
        "code",
    ] {
        if let Some(value) = node.attributes.get(name) {
            return Some(value.trim().to_owned());
        }
        if let Some(child) = node
            .children
            .iter()
            .find(|child| normalize(&child.name) == name)
        {
            if !child.text.trim().is_empty() {
                return Some(child.text.trim().to_owned());
            }
        }
    }
    if matches!(
        normalize(&node.name).as_str(),
        "item" | "product" | "entry" | "gear" | "equipment"
    ) {
        return node.attributes.get("id").cloned().or_else(|| {
            node.children
                .iter()
                .find(|child| normalize(&child.name) == "id")
                .map(|child| child.text.trim().to_owned())
                .filter(|value| !value.is_empty())
        });
    }
    None
}

fn xml_node_category_value(node: &XmlNode) -> String {
    node.attributes
        .get("name")
        .or_else(|| node.attributes.get("id"))
        .or_else(|| node.attributes.get("code"))
        .map(String::as_str)
        .unwrap_or(node.text.trim())
        .to_owned()
}

fn collect_item_categories(node: &XmlNode, output: &mut Vec<String>) {
    for child in &node.children {
        if is_category_name(&child.name) {
            let value = xml_node_category_value(child);
            if !value.is_empty() {
                output.push(value);
            }
            for value in ["category", "categories"]
                .iter()
                .filter_map(|attribute| child.attributes.get(*attribute))
            {
                output.extend(value.split([',', ';', '|']).map(str::to_owned));
            }
        } else {
            collect_item_categories(child, output);
        }
    }
    for value in ["category", "categories"]
        .iter()
        .filter_map(|attribute| node.attributes.get(*attribute))
    {
        output.extend(value.split([',', ';', '|']).map(str::to_owned));
    }
}

fn xml_category_name(category: &XmlCategory) -> String {
    normalize(
        category
            .name
            .as_deref()
            .or_else(|| (!category.text.trim().is_empty()).then_some(category.text.trim()))
            .or(category.id.as_deref())
            .or(category.code.as_deref())
            .unwrap_or_default(),
    )
}

fn xml_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
}

async fn get_rates(client: &reqwest::Client, url: &Url) -> Result<Rates> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {url} response"))?;
    if !status.is_success() {
        bail!("GET {url} returned {status}: {body}");
    }
    parse_rates(&body).with_context(|| format!("GET {url} returned invalid rates YAML"))
}

fn parse_rates(yaml: &str) -> Result<Rates> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml).context("Rates document is not valid YAML")?;
    let version = yaml_field(&value, "version")
        .and_then(yaml_text)
        .context("Rates YAML has no version")?;
    let rate_root = yaml_field(&value, "rates").unwrap_or(&value);
    let mut huf_per_unit = HashMap::new();
    if let Some(map) = rate_root.as_mapping() {
        for (currency, rate) in map {
            let Some(currency) = yaml_text(currency) else {
                continue;
            };
            let currency = currency.to_ascii_uppercase();
            if matches!(currency.as_str(), "EUR" | "USD" | "HUF") {
                let rate = yaml_number(rate)
                    .or_else(|| yaml_first_number(rate, &["huf", "toHuf", "rate", "value"]))
                    .with_context(|| format!("Rates YAML has an invalid {currency} rate"))?;
                if !rate.is_finite() || rate <= 0.0 {
                    bail!("Rates YAML has an invalid {currency} rate: {rate}");
                }
                huf_per_unit.insert(currency, rate);
            }
        }
    }
    if let Some(rows) = rate_root.as_sequence() {
        for row in rows {
            let currency = yaml_field(row, "currency")
                .or_else(|| yaml_field(row, "code"))
                .and_then(yaml_text)
                .context("Rate entry has no currency")?
                .to_ascii_uppercase();
            let rate = yaml_first_number(row, &["huf", "toHuf", "rate", "value"])
                .context("Rate entry has no valid rate")?;
            if !rate.is_finite() || rate <= 0.0 {
                bail!("Rate entry has an invalid rate: {rate}");
            }
            huf_per_unit.insert(currency, rate);
        }
    }
    huf_per_unit.insert("HUF".to_owned(), 1.0);
    if !huf_per_unit.contains_key("EUR") && !huf_per_unit.contains_key("USD") {
        bail!("Rates YAML contains neither EUR nor USD rates");
    }
    Ok(Rates {
        version,
        huf_per_unit,
    })
}

fn yaml_field<'a>(value: &'a serde_yaml::Value, name: &str) -> Option<&'a serde_yaml::Value> {
    value.as_mapping()?.iter().find_map(|(key, value)| {
        yaml_text(key)
            .is_some_and(|key| normalize(&key) == normalize(name))
            .then_some(value)
    })
}

fn yaml_text(value: &serde_yaml::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
        .or_else(|| value.as_u64().map(|number| number.to_string()))
        .or_else(|| value.as_f64().map(|number| number.to_string()))
}

fn yaml_number(value: &serde_yaml::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn yaml_first_number(value: &serde_yaml::Value, fields: &[&str]) -> Option<f64> {
    fields
        .iter()
        .find_map(|field| yaml_field(value, field).and_then(yaml_number))
}

fn find_array<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    if let Some(array) = value.as_array() {
        return Some(array);
    }
    let object = value.as_object()?;
    for (key, child) in object {
        if keys
            .iter()
            .any(|candidate| normalize(key) == normalize(candidate))
        {
            if let Some(array) = child.as_array() {
                return Some(array);
            }
        }
    }
    object.get("data").and_then(|data| find_array(data, keys))
}

fn find_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = value.as_object()?;
    for (key, child) in object {
        if keys
            .iter()
            .any(|candidate| normalize(key) == normalize(candidate))
        {
            return Some(child);
        }
    }
    object.get("data").and_then(|data| find_field(data, keys))
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| find_field(value, &[*key]).and_then(value_text))
}

fn value_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_number().map(ToString::to_string))
}

fn number_field(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| find_field(value, &[*key]).and_then(number_value))
}

fn integer_field(value: &Value, keys: &[&str]) -> Option<i64> {
    let number = number_field(value, keys)?;
    if !number.is_finite()
        || number.fract() != 0.0
        || number < i64::MIN as f64
        || number >= i64::MAX as f64
    {
        return None;
    }
    Some(number as i64)
}

fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

fn parse_weight_text(value: &str) -> Result<(f64, String)> {
    let end = value
        .char_indices()
        .find(|(_, character)| {
            !character.is_ascii_digit() && *character != '.' && *character != ','
        })
        .map(|(index, _)| index)
        .context("Weight text has no unit")?;
    let amount = value[..end]
        .trim()
        .replace(',', ".")
        .parse()
        .context("Weight text has an invalid amount")?;
    Ok((amount, value[end..].trim().to_owned()))
}

fn sku_id(sku: &str) -> Result<u64> {
    let digits = sku.chars().filter(char::is_ascii_digit).collect::<String>();
    digits
        .parse()
        .with_context(|| format!("SKU {sku:?} has no valid numeric identifier"))
}

fn normalize(value: &str) -> String {
    let mut normalized = String::new();
    for character in value.chars().flat_map(char::to_lowercase) {
        let replacement = match character {
            'á' | 'à' | 'ä' | 'â' => Some('a'),
            'é' | 'è' | 'ë' | 'ê' => Some('e'),
            'í' | 'ì' | 'ï' | 'î' => Some('i'),
            'ó' | 'ò' | 'ö' | 'ő' | 'ô' => Some('o'),
            'ú' | 'ù' | 'ü' | 'ű' | 'û' => Some('u'),
            'ñ' => Some('n'),
            ascii if ascii.is_ascii_alphanumeric() => Some(ascii),
            _ => None,
        };
        if let Some(character) = replacement {
            normalized.push(character);
        }
    }
    normalized
}

fn round_half_up(value: f64) -> Result<u64> {
    if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 {
        bail!("Cannot round invalid or out-of-range value {value}");
    }
    Ok((value + 0.5).floor() as u64)
}

fn optimize(
    candidates: &[Candidate],
    budget: u64,
    weight_limit: u64,
    required_mask: u64,
) -> Option<Vec<usize>> {
    let mut states: HashMap<u64, Vec<State>> = HashMap::new();
    states.insert(
        0,
        vec![State {
            cost: 0,
            weight: 0,
            points: 0,
            selected: Vec::new(),
        }],
    );
    for (index, item) in candidates.iter().enumerate() {
        let previous = states.clone();
        for (mask, previous_states) in previous {
            for state in previous_states {
                let cost = state.cost.checked_add(item.price)?;
                let weight = state.weight.checked_add(item.weight)?;
                if cost > budget || weight > weight_limit {
                    continue;
                }
                let points = state.points + item.points;
                let next_mask = mask | item.categories;
                let next = State {
                    cost,
                    weight,
                    points,
                    selected: {
                        let mut selected = state.selected;
                        selected.push(index);
                        selected
                    },
                };
                let frontier = states.entry(next_mask).or_default();
                if frontier.iter().any(|old| dominates(old, &next)) {
                    continue;
                }
                frontier.retain(|old| !dominates(&next, old));
                frontier.push(next);
            }
        }
    }
    states
        .iter()
        .filter(|(mask, _)| **mask & required_mask == required_mask)
        .flat_map(|(_, frontier)| frontier.iter())
        .max_by_key(|state| state.points)
        .map(|state| state.selected.clone())
}

fn dominates(left: &State, right: &State) -> bool {
    left.cost <= right.cost && left.weight <= right.weight && left.points >= right.points
}

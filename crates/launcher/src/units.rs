//! Unit conversion for the "=" mode: "10 km in mi", "100 f to c", "2 gib in mb".

/// (names, quantity, factor to the base unit). Temperatures are special.
const UNITS: &[(&[&str], &str, f64)] = &[
    (&["mm"], "length", 0.001),
    (&["cm"], "length", 0.01),
    (&["m"], "length", 1.0),
    (&["km"], "length", 1000.0),
    (&["in", "inch", "\""], "length", 0.0254),
    (&["ft", "feet", "foot"], "length", 0.3048),
    (&["yd"], "length", 0.9144),
    (&["mi", "mile", "miles"], "length", 1609.344),
    (&["nmi"], "length", 1852.0),
    (&["mg"], "mass", 0.001),
    (&["g"], "mass", 1.0),
    (&["kg"], "mass", 1000.0),
    (&["t"], "mass", 1_000_000.0),
    (&["oz"], "mass", 28.349_523_125),
    (&["lb", "lbs"], "mass", 453.592_37),
    (&["ml"], "volume", 0.001),
    (&["cl"], "volume", 0.01),
    (&["l"], "volume", 1.0),
    (&["m3"], "volume", 1000.0),
    (&["gal"], "volume", 3.785_411_784),
    (&["floz"], "volume", 0.029_573_529_6),
    (&["cup"], "volume", 0.236_588_236_5),
    (&["ms"], "time", 0.001),
    (&["s", "sec"], "time", 1.0),
    (&["min"], "time", 60.0),
    (&["h", "hr"], "time", 3600.0),
    (&["d", "day", "days"], "time", 86400.0),
    (&["w", "week", "weeks"], "time", 604_800.0),
    (&["y", "year", "years"], "time", 31_557_600.0),
    (&["b", "byte"], "data", 1.0),
    (&["kb"], "data", 1e3),
    (&["mb"], "data", 1e6),
    (&["gb"], "data", 1e9),
    (&["tb"], "data", 1e12),
    (&["kib"], "data", 1024.0),
    (&["mib"], "data", 1_048_576.0),
    (&["gib"], "data", 1_073_741_824.0),
    (&["tib"], "data", 1_099_511_627_776.0),
    (&["m/s"], "speed", 1.0),
    (&["km/h", "kmh", "kph"], "speed", 1.0 / 3.6),
    (&["mph"], "speed", 0.447_04),
    (&["kn", "kt"], "speed", 0.514_444),
    (&["m2"], "area", 1.0),
    (&["km2"], "area", 1e6),
    (&["ha"], "area", 1e4),
    (&["acre"], "area", 4046.856_422_4),
    (&["ft2"], "area", 0.092_903_04),
    (&["j"], "energy", 1.0),
    (&["kj"], "energy", 1000.0),
    (&["cal"], "energy", 4.184),
    (&["kcal"], "energy", 4184.0),
    (&["wh"], "energy", 3600.0),
    (&["kwh"], "energy", 3_600_000.0),
];

fn unit(name: &str) -> Option<(&'static str, f64)> {
    let n = name.to_lowercase();
    UNITS.iter().find(|(names, ..)| names.contains(&n.as_str())).map(|(_, q, f)| (*q, *f))
}

fn to_kelvin(v: f64, u: &str) -> Option<f64> {
    match u {
        "c" | "°c" => Some(v + 273.15),
        "f" | "°f" => Some((v - 32.0) * 5.0 / 9.0 + 273.15),
        "k" => Some(v),
        _ => None,
    }
}

fn from_kelvin(k: f64, u: &str) -> Option<f64> {
    match u {
        "c" | "°c" => Some(k - 273.15),
        "f" | "°f" => Some((k - 273.15) * 9.0 / 5.0 + 32.0),
        "k" => Some(k),
        _ => None,
    }
}

/// "<expression> <unit> in|to <unit>": the converted value and its unit.
pub fn convert(query: &str) -> Option<(f64, String)> {
    let lower = query.to_lowercase();
    let (left, target) = lower.rsplit_once(" in ").or_else(|| lower.rsplit_once(" to "))?;
    let target = target.trim();
    let left = left.trim();
    let split = left.rfind(|c: char| c.is_ascii_digit() || c == ')' || c == '.')? + 1;
    let (expr, from) = (left[..split].trim(), left[split..].trim());
    let value = crate::calc::eval(expr).ok()?;
    if let (Some(k), true) = (to_kelvin(value, from), from_kelvin(0.0, target).is_some()) {
        return Some((from_kelvin(k, target)?, target.to_string()));
    }
    let (qa, fa) = unit(from)?;
    let (qb, fb) = unit(target)?;
    (qa == qb).then(|| (value * fa / fb, target.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let near = |q: &str, want: f64| {
            let (v, _) = convert(q).unwrap_or_else(|| panic!("{q}"));
            assert!((v - want).abs() < 1e-6 * want.abs().max(1.0), "{q}: {v}");
        };
        near("10 km in mi", 6.213_711_922);
        near("100 f to c", 37.777_777_778);
        near("2 gib in mb", 2147.483_648);
        near("(1+1) h in min", 120.0);
        near("90 km/h in m/s", 25.0);
        assert!(convert("3 kg in m").is_none());
        assert!(convert("3 + 4").is_none());
    }
}

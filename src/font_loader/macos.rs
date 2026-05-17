// The MIT License (MIT)
// Copyright (c) font-loader Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
// associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute,
// sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or
// substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT
// NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

/// Font loading utilities for installed system fonts
pub mod system_fonts {
    use core_text::font_descriptor::*;
    use core_text::font_descriptor::{self, TraitAccessors};
    use core_text;
    use freetype;
    use std::fs::File;
    use std::mem;
    use std::ptr;
    use core_foundation::string::CFString;
    use core_foundation::number::CFNumber;
    use core_foundation::array::CFArray;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::url::CFURL;
    use libc::c_int;
    use std::io::Read;
    /// The platform specific font properties
    pub type FontProperty = CTFontDescriptor;

    /// Builder for FontProperty
    pub struct FontPropertyBuilder {
        symbolic_traits: CTFontSymbolicTraits,
        family: String
    }

    impl FontPropertyBuilder {
        pub fn new() -> FontPropertyBuilder {
            FontPropertyBuilder{ symbolic_traits: 0, family: String::new()}
        }

        pub fn italic(mut self) -> FontPropertyBuilder {
            self.symbolic_traits |= kCTFontItalicTrait;
            self
        }

        pub fn oblique(self) -> FontPropertyBuilder {
            self.italic()
        }

        pub fn monospace(mut self) -> FontPropertyBuilder {
            self.symbolic_traits |= kCTFontMonoSpaceTrait;
            self
        }

        pub fn bold(mut self) -> FontPropertyBuilder {
            self.symbolic_traits |= kCTFontBoldTrait;
            self
        }

        pub fn family(mut self, name: &str) -> FontPropertyBuilder {
            self.family = name.to_string();
            self
        }

        pub fn build(self) -> FontProperty {
            let family_attr: CFString = unsafe { TCFType::wrap_under_get_rule(kCTFontFamilyNameAttribute) };
            let family_name: CFString = self.family.parse().unwrap();
            let traits_attr: CFString = unsafe { TCFType::wrap_under_get_rule(kCTFontTraitsAttribute) };
            let symbolic_traits_attr: CFString = unsafe { TCFType::wrap_under_get_rule(kCTFontSymbolicTrait) };
            let traits = CFDictionary::from_CFType_pairs(&[(symbolic_traits_attr.as_CFType(), CFNumber::from(self.symbolic_traits as i32).as_CFType())]);
            let mut attributes = Vec::new();
            attributes.push((traits_attr, traits.as_CFType()));
            if self.family.len() != 0 {
                attributes.push((family_attr, family_name.as_CFType()));
            }
            let attributes = CFDictionary::from_CFType_pairs(&attributes);
            font_descriptor::new_from_attributes(&attributes)
        }
    }

    /// Get the binary data and index of a specific font
    pub fn get(config: &FontProperty) -> Option<(Vec<u8>, c_int)> {
        let mut buffer = Vec::new();
        let url: CFURL;
        unsafe {
            let value =
                CTFontDescriptorCopyAttribute(config.as_concrete_TypeRef(), kCTFontURLAttribute);

            if value.is_null() {
                return None
            }

            let value: CFType = TCFType::wrap_under_get_rule(value);
            if !value.instance_of::<CFURL>() {
                return None
            }
            url = TCFType::wrap_under_get_rule(mem::transmute(value.as_CFTypeRef()));
        }
        if let Some(path) = url.to_path() {
            match File::open(path).and_then(|mut f| f.read_to_end(&mut buffer)) {
                Ok(_) => return Some((buffer, 0)),
                Err(_) => return None,
            }
        };
        return None
    }

    /// Strictly match a family + style. Walks all matching descriptors
    /// and returns the bytes + face index of the first whose actual
    /// content really has the requested bold/italic combination,
    /// verified against FreeType's own `style_flags`.
    ///
    /// Core Text's trait match is loose: it considers SemiBold "bold-
    /// enough" via `kCTFontBoldTrait`, so a request for bold+italic
    /// against "Iosevka Term" returns `SGr-IosevkaTerm-SemiBold.ttc`
    /// first — but no face inside that TTC has FT's BOLD style flag.
    /// The actual Bold-Italic face lives in `SGr-IosevkaTerm-Bold.ttc`
    /// at face index 4. Trusting Core Text's first match here meant
    /// bold-italic silently rendered as semi-bold regular.
    ///
    /// Now we filter by FT style flags across every matching
    /// descriptor's file. The first file that contains an exact-match
    /// face wins, and the returned `c_int` is the face index inside
    /// that TTC so callers can hand it straight to
    /// `library.new_memory_face(data, index)`.
    pub fn get_strict(family: &str, bold: bool, italic: bool) -> Option<(Vec<u8>, c_int)> {
        let mut want: CTFontSymbolicTraits = 0;
        if bold { want |= kCTFontBoldTrait; }
        if italic { want |= kCTFontItalicTrait; }

        let mut prop = FontPropertyBuilder::new().family(family);
        if bold { prop = prop.bold(); }
        if italic { prop = prop.italic(); }
        let descriptor = prop.build();

        let descs: CFArray<CTFontDescriptor> = unsafe {
            let descs = CTFontDescriptorCreateMatchingFontDescriptors(
                descriptor.as_concrete_TypeRef(),
                ptr::null(),
            );
            if descs.is_null() {
                return None;
            }
            TCFType::wrap_under_create_rule(descs)
        };

        let mask = kCTFontBoldTrait | kCTFontItalicTrait;
        // Track files we've already loaded so we don't open Iosevka's
        // 16-descriptor bold-italic list 16 times.
        let mut tried_paths: std::collections::BTreeSet<std::path::PathBuf> =
            std::collections::BTreeSet::new();
        // Fallback: if FT-side verification finds no exact match in
        // any candidate, return the first readable file at face 0 so
        // the user gets *something*. Better than a hard load failure.
        let mut fallback: Option<(Vec<u8>, c_int)> = None;
        for desc in descs.iter() {
            let traits = desc.traits().symbolic_traits();
            if traits & mask != want {
                continue;
            }
            let url: CFURL = unsafe {
                let value = CTFontDescriptorCopyAttribute(
                    desc.as_concrete_TypeRef(),
                    kCTFontURLAttribute,
                );
                if value.is_null() {
                    continue;
                }
                let value: CFType = TCFType::wrap_under_create_rule(value);
                if !value.instance_of::<CFURL>() {
                    continue;
                }
                TCFType::wrap_under_get_rule(mem::transmute(value.as_CFTypeRef()))
            };
            let Some(path) = url.to_path() else { continue };
            if !tried_paths.insert(path.clone()) {
                continue;
            }
            let mut buffer = Vec::new();
            if File::open(&path).and_then(|mut f| f.read_to_end(&mut buffer)).is_err() {
                continue;
            }
            if let Some(idx) = ft_face_index_for_style(&buffer, bold, italic) {
                return Some((buffer, idx));
            }
            if fallback.is_none() {
                fallback = Some((buffer, 0));
            }
        }
        fallback
    }

    /// Scan a TTC's faces with FreeType, returning the index of the
    /// first one whose `style_flags` match the requested bold/italic
    /// combination. Returns `None` when no packed face has the right
    /// flags — that's the signal for `get_strict` to try the next
    /// candidate file rather than fall through to a regular-face
    /// substitution.
    fn ft_face_index_for_style(data: &[u8], want_bold: bool, want_italic: bool) -> Option<c_int> {
        let library = freetype::Library::init().ok()?;
        let probe = library.new_memory_face(data.to_vec(), 0).ok()?;
        let n = probe.num_faces() as isize;
        for i in 0..n {
            let face = match library.new_memory_face(data.to_vec(), i) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let flags = face.style_flags();
            let bold = flags.contains(freetype::face::StyleFlag::BOLD);
            let italic = flags.contains(freetype::face::StyleFlag::ITALIC);
            if bold == want_bold && italic == want_italic {
                return Some(i as c_int);
            }
        }
        None
    }

    /// Query the names of all fonts installed in the system
    pub fn query_all() -> Vec<String> {
        core_text::font_collection::get_family_names()
            .iter()
            .map(|family_name| family_name.to_string())
            .collect()
    }

    /// Query the names of specifc fonts installed in the system
    pub fn query_specific(property: &mut FontProperty) -> Vec<String> {
        let descs: CFArray<CTFontDescriptor> = unsafe {
            let descs = CTFontDescriptorCreateMatchingFontDescriptors(
                property.as_concrete_TypeRef(),
                ptr::null(),
            );
            TCFType::wrap_under_create_rule(descs)
        };
        descs
            .iter()
            .map(|desc| desc.family_name())
            .collect::<Vec<_>>()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Resolve `(family, bold, italic)` through `get_strict`, open
        /// the returned face with FT, and assert its style flags
        /// strictly match. Skips if the family isn't installed.
        fn assert_strict_match(family: &str, bold: bool, italic: bool) {
            let Some((data, idx)) = get_strict(family, bold, italic) else {
                eprintln!("skipping: {family} {bold}/{italic} not installed");
                return;
            };
            let library = freetype::Library::init().expect("ft init");
            let face = library
                .new_memory_face(data, idx as isize)
                .unwrap_or_else(|e| panic!("ft open at idx {idx}: {e}"));
            let flags = face.style_flags();
            let got_bold = flags.contains(freetype::face::StyleFlag::BOLD);
            let got_italic = flags.contains(freetype::face::StyleFlag::ITALIC);
            assert_eq!(
                (got_bold, got_italic),
                (bold, italic),
                "get_strict({family:?}, bold={bold}, italic={italic}) returned a face \
                 with the wrong FT style flags (idx={idx} style={:?})",
                face.style_name(),
            );
        }

        #[test]
        fn get_strict_iosevka_term_all_combinations() {
            // Regression for the bold-italic bug. Iosevka Term's
            // bold-italic face lives at index 4 of
            // `SGr-IosevkaTerm-Bold.ttc`. Core Text's first match for
            // bold+italic on this family is the SemiBold.ttc instead
            // (semi-bold satisfies kCTFontBoldTrait), so a naive
            // implementation that trusts CoreText's first descriptor
            // returns a semibold-italic file whose faces have FT's
            // ITALIC flag but not BOLD — meaning the user sees italic
            // semi-bold instead of full bold-italic. `get_strict`
            // must verify against FT and walk to the next candidate
            // file when needed.
            //
            // Skips when Iosevka Term isn't installed (CI machines).
            for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
                assert_strict_match("Iosevka Term", bold, italic);
            }
        }

        #[test]
        fn ft_face_index_for_style_returns_none_for_empty_data() {
            // Defensive: garbage data must not panic; returning None
            // signals get_strict to try the next candidate file.
            assert_eq!(ft_face_index_for_style(&[], true, true), None);
        }
    }
}

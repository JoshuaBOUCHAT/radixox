#[repr(align(64))]
#[derive(Default)]
pub struct BitBlock {
    pub data: [u64; 8], // 512 bits
}

impl BitBlock {
    /// Bits posés du bloc.
    #[inline]
    pub fn popcnt(&self) -> u32 {
        self.data.iter().map(|w| w.count_ones()).sum()
    }

    #[inline(always)]
    pub fn is_full(&self) -> bool {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
        unsafe {
            self.is_full_x86_512()
        }

        #[cfg(all(
            target_arch = "x86_64",
            target_feature = "avx2",
            not(target_feature = "avx512f")
        ))]
        unsafe {
            self.is_full_x86_256()
        }

        #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
        unsafe {
            self.is_full_arm_neon()
        }

        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "avx512f"),
            all(target_arch = "x86_64", target_feature = "avx2"),
            all(target_arch = "aarch64", target_feature = "neon")
        )))]
        {
            self.data.iter().all(|&x| x == u64::MAX)
        }
    }

    // --- x86_64 AVX-512 ---
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    #[target_feature(enable = "avx512f")]
    unsafe fn is_full_x86_512(&self) -> bool {
        use core::arch::x86_64::*;
        unsafe {
            let data = _mm512_load_si512(self.data.as_ptr() as *const __m512i);
            let full = _mm512_set1_epi64(-1);
            let mask = _mm512_cmpeq_epu64_mask(data, full);
            mask == 0xFF
        }
    }

    // --- x86_64 AVX2 ---
    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx2",
        not(target_feature = "avx512f")
    ))]
    #[target_feature(enable = "avx2")]
    unsafe fn is_full_x86_256(&self) -> bool {
        use core::arch::x86_64::*;
        let ptr = self.data.as_ptr() as *const __m256i;
        let full = _mm256_set1_epi64x(-1);

        // On check deux blocs de 256 bits
        unsafe {
            let res1 = _mm256_testc_si256(_mm256_load_si256(ptr), full);
            let res2 = _mm256_testc_si256(_mm256_load_si256(ptr.add(1)), full);
            (res1 != 0) && (res2 != 0)
        }
    }

    // --- ARM64 NEON ---
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    #[target_feature(enable = "neon")]
    unsafe fn is_full_arm_neon(&self) -> bool {
        use core::arch::aarch64::*;
        let ptr = self.data.as_ptr();

        // NEON travaille sur 128 bits (2 x u64), on fait donc 4 loads
        // pour couvrir les 8 x u64 du BitBlock
        let v1 = vld1q_u64(ptr); // data[0..2]
        let v2 = vld1q_u64(ptr.add(2)); // data[2..4]
        let v3 = vld1q_u64(ptr.add(4)); // data[4..6]
        let v4 = vld1q_u64(ptr.add(6)); // data[6..8]

        let res = vandq_u64(vandq_u64(v1, v2), vandq_u64(v3, v4));
        // vgetq_lane_u64 extrait un u64 du registre NEON
        vgetq_lane_u64(res, 0) == u64::MAX && vgetq_lane_u64(res, 1) == u64::MAX
    }
}
impl BitBlock {
    #[inline(always)]
    pub fn find_first_free(&self) -> Option<usize> {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
        unsafe {
            self.find_x86_512()
        }

        #[cfg(all(
            target_arch = "x86_64",
            target_feature = "avx2",
            not(target_feature = "avx512f")
        ))]
        unsafe {
            self.find_x86_256()
        }

        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "avx512f"),
            all(target_arch = "x86_64", target_feature = "avx2")
        )))]
        {
            for (i, &word) in self.data.iter().enumerate() {
                if word != u64::MAX {
                    let bit_idx = (!word).trailing_zeros() as usize;
                    return Some(i * 64 + bit_idx);
                }
            }
            None
        }
    }

    // --- x86_64 AVX-512 ---
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    unsafe fn find_x86_512(&self) -> Option<usize> {
        use core::arch::x86_64::*;
        unsafe {
            let data = _mm512_load_si512(self.data.as_ptr() as *const __m512i);
            let full = _mm512_set1_epi64(-1);

            // On cherche les éléments qui NE sont PAS égaux à u64::MAX
            let mask = _mm512_cmpneq_epu64_mask(data, full);

            if mask == 0 {
                return None;
            }

            // Trouver quel u64 parmi les 8 a un trou
            let u64_idx = mask.trailing_zeros() as usize;
            let word = *self.data.get_unchecked(u64_idx);
            let bit_idx = (!word).trailing_zeros() as usize;

            Some(u64_idx * 64 + bit_idx)
        }
    }

    // --- x86_64 AVX2 ---
    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx2",
        not(target_feature = "avx512f")
    ))]
    unsafe fn find_x86_256(&self) -> Option<usize> {
        use core::arch::x86_64::*;
        let ptr = self.data.as_ptr() as *const __m256i;
        unsafe {
            let full = _mm256_set1_epi64x(-1);

            // On teste les deux moitiés de 256 bits
            for i in 0..2 {
                let chunk = _mm256_load_si256(ptr.add(i));
                // Si PTEST indique que tout n'est pas à 1
                if _mm256_testc_si256(chunk, full) == 0 {
                    // On cherche manuellement dans les 4 u64 de ce chunk
                    for j in (i * 4)..(i * 4 + 4) {
                        let word = *self.data.get_unchecked(j);
                        if word != u64::MAX {
                            return Some(j * 64 + (!word).trailing_zeros() as usize);
                        }
                    }
                }
            }
            None
        }
    }
}
impl BitBlock {
    #[inline(always)]
    pub fn set_bit_and_check_full(&mut self, bit_idx: usize) -> bool {
        debug_assert!(bit_idx < 512);
        let word_idx = bit_idx >> 6;
        let bit = bit_idx & 63;

        unsafe {
            let word = self.data.get_unchecked_mut(word_idx);
            *word |= 1 << bit;

            // SHORT-CIRCUIT:
            // Si le mot de 64 bits n'est pas plein, le bloc de 512 ne peut pas l'être.
            if *word != u64::MAX {
                return false;
            }
        }

        // Si on arrive ici, le mot actuel est plein.
        // On check les 7 autres via ta fonction SIMD ou scalaire optimisée.
        self.is_full()
    }
    #[inline(always)]
    pub fn clear_bit_and_was_full(&mut self, bit_idx: usize) -> bool {
        debug_assert!(bit_idx < 512);
        let word_idx = bit_idx >> 6;
        let bit = bit_idx & 63;
        let was_full = self.is_full();

        unsafe {
            let word = self.data.get_unchecked_mut(word_idx);
            let bit_mask = 1 << bit;

            // On vérifie si le bit était à 1 avant de le mettre à 0
            // ET si le bloc entier était marqué plein via ton check SIMD

            *word &= !bit_mask;
        }
        was_full
    }
    #[inline(always)]
    pub fn is_set(&self, bit_idx: usize) -> bool {
        debug_assert!(bit_idx < 512);
        let word_idx = bit_idx >> 6;
        let word = self.data[word_idx];
        let mask = 1 << (bit_idx & 63);
        word & mask != 0
    }
}

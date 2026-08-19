#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cert_dir="${script_dir}/generated/certs"
config_dir="${script_dir}/generated/config"
authorization_dir="${script_dir}/generated/authorization"
state_dir="${script_dir}/generated/state"
offline_dir="${script_dir}/generated/offline"
offline_home2_dir="${script_dir}/generated/offline-home2"
travel_dir="${script_dir}/generated/travel"
mkdir -p "${cert_dir}"
mkdir -p "${config_dir}"
mkdir -p "${authorization_dir}"
mkdir -p "${state_dir}"
mkdir -p "${offline_dir}"
mkdir -p "${offline_home2_dir}"
rm -rf "${travel_dir}"
find "${cert_dir}" -maxdepth 1 -type f -delete
find "${config_dir}" -maxdepth 1 -type f -delete
find "${authorization_dir}" -maxdepth 1 -type f -delete
find "${state_dir}" -maxdepth 1 -type f -delete
find "${offline_dir}" -maxdepth 1 -type f -delete
find "${offline_home2_dir}" -maxdepth 1 -type f -delete
password_file="${offline_dir}/test-password.txt"
printf '%s\n' 'flowsplice-e2e-private-key-password' >"${password_file}"
chmod 600 "${password_file}"

make_ca() {
  local name="$1"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -sha256 -nodes \
    -days 730 -subj "/CN=FlowSplice ${name}" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -keyout "${offline_dir}/${name}.plain.key" -out "${cert_dir}/${name}.crt" >/dev/null 2>&1
  openssl pkcs8 -topk8 -v2 aes-256-cbc \
    -in "${offline_dir}/${name}.plain.key" \
    -passout "file:${password_file}" \
    -out "${offline_dir}/${name}.key" >/dev/null 2>&1
  rm -f "${offline_dir}/${name}.plain.key"
}

issue() {
  local name="$1" role="$2" id="$3" dns="$4" eku="$5" ca="$6"
  local ext="${cert_dir}/${name}.ext"
  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -sha256 -nodes \
    -subj "/CN=${name}" -keyout "${cert_dir}/${name}.key" \
    -out "${cert_dir}/${name}.csr" >/dev/null 2>&1
  {
    printf 'basicConstraints=critical,CA:FALSE\n'
    printf 'keyUsage=critical,digitalSignature\n'
    printf 'extendedKeyUsage=%s\n' "${eku}"
    printf 'subjectAltName=URI:flowsplice://identity/%s/%s,DNS:%s\n' "${role}" "${id}" "${dns}"
  } >"${ext}"
  openssl x509 -req -sha256 -days 30 -in "${cert_dir}/${name}.csr" \
    -CA "${cert_dir}/${ca}.crt" -CAkey "${offline_dir}/${ca}.key" \
    -passin "file:${password_file}" -CAcreateserial \
    -extfile "${ext}" -out "${cert_dir}/${name}.crt" >/dev/null 2>&1
  openssl verify -CAfile "${cert_dir}/${ca}.crt" "${cert_dir}/${name}.crt" >/dev/null
  openssl x509 -in "${cert_dir}/${name}.crt" -noout -ext subjectAltName \
    | grep -F "URI:flowsplice://identity/${role}/${id}" >/dev/null
}

make_ca management-ca
make_ca business-ca
issue server server server-1 server.flowsplice serverAuth,clientAuth management-ca
issue relay1 relay relay-1 relay-1.flowsplice serverAuth management-ca
issue relay2 relay relay-2 relay-2.flowsplice serverAuth management-ca
issue home-management home home-1 home-management.flowsplice clientAuth management-ca
issue home-business home home-1 home-1.flowsplice serverAuth business-ca
issue home2-management home home-2 home2-management.flowsplice clientAuth management-ca
issue home2-business home home-2 home-2.flowsplice serverAuth business-ca
for authority in home1-authority home2-authority global-authority; do
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
    -aes-256-cbc -pass "file:${password_file}" \
    -out "${offline_dir}/${authority}.key" >/dev/null 2>&1
  python3 "${script_dir}/authority-public-key.py" \
    --authority-key "${offline_dir}/${authority}.key" \
    --password-file "${password_file}" \
    --output "${authorization_dir}/${authority}-public-key.txt"
done

# Each Home owns a separate writable issuer directory. Copy the shared CA keys
# into Home 2's test-only directory so rotating one Home cannot mutate another
# Home's local key files.
cp "${offline_dir}/management-ca.key" "${offline_home2_dir}/management-ca.key"
cp "${offline_dir}/business-ca.key" "${offline_home2_dir}/business-ca.key"
cp "${offline_dir}/home2-authority.key" "${offline_home2_dir}/home2-authority.key"
cp "${password_file}" "${offline_home2_dir}/test-password.txt"

find "${cert_dir}" -maxdepth 1 \( -name '*.csr' -o -name '*.ext' -o -name '*.srl' \) -delete
chmod 600 "${cert_dir}"/*.key
chmod 600 "${offline_dir}"/*.key
chmod 600 "${offline_home2_dir}"/*.key "${offline_home2_dir}/test-password.txt"
chmod 644 "${cert_dir}"/*.crt

spki_pin() {
  openssl x509 -in "${cert_dir}/$1.crt" -pubkey -noout \
    | openssl pkey -pubin -outform DER 2>/dev/null \
    | openssl dgst -sha256 \
    | sed 's/^.*= //'
}

server_pin="$(spki_pin server)"
relay1_pin="$(spki_pin relay1)"
relay2_pin="$(spki_pin relay2)"
home_management_pin="$(spki_pin home-management)"
home_business_pin="$(spki_pin home-business)"
home2_management_pin="$(spki_pin home2-management)"
home2_business_pin="$(spki_pin home2-business)"
home1_authority_public_key="$(tr -d '\n' <"${authorization_dir}/home1-authority-public-key.txt")"
home2_authority_public_key="$(tr -d '\n' <"${authorization_dir}/home2-authority-public-key.txt")"
global_authority_public_key="$(tr -d '\n' <"${authorization_dir}/global-authority-public-key.txt")"
printf '{"credentials":[]}\n' >"${authorization_dir}/credentials.json"

docker run --rm \
  --user "$(id -u):$(id -g)" \
  -e FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE=1 \
  -v "${script_dir}/generated:/generated" \
  flowsplice-e2e:local \
  /usr/local/bin/flowsplice-travelagent enroll-init \
  --travel-id travel-1 \
  --output-dir /generated/travel \
  --test-password-file /generated/offline/test-password.txt
cp "${password_file}" "${travel_dir}/test-password.txt"
cp "${travel_dir}/enrollment-request.json" "${authorization_dir}/enrollment-request.json"
chmod 600 "${travel_dir}/test-password.txt"

for template in "${script_dir}"/config/*.toml; do
  output="${config_dir}/$(basename -- "${template}")"
  sed \
    -e "s/__SERVER_PIN__/${server_pin}/g" \
    -e "s/__RELAY1_PIN__/${relay1_pin}/g" \
    -e "s/__RELAY2_PIN__/${relay2_pin}/g" \
    -e "s/__HOME_MANAGEMENT_PIN__/${home_management_pin}/g" \
    -e "s/__HOME_BUSINESS_PIN__/${home_business_pin}/g" \
    -e "s/__HOME2_MANAGEMENT_PIN__/${home2_management_pin}/g" \
    -e "s/__HOME2_BUSINESS_PIN__/${home2_business_pin}/g" \
    -e "s/__HOME1_AUTHORITY_PUBLIC_KEY__/${home1_authority_public_key}/g" \
    -e "s/__HOME2_AUTHORITY_PUBLIC_KEY__/${home2_authority_public_key}/g" \
    -e "s/__GLOBAL_AUTHORITY_PUBLIC_KEY__/${global_authority_public_key}/g" \
    "${template}" >"${output}"
done
printf 'Generated E2E certificates in %s\n' "${cert_dir}"

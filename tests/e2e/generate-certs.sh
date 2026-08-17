#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cert_dir="${script_dir}/generated/certs"
config_dir="${script_dir}/generated/config"
mkdir -p "${cert_dir}"
mkdir -p "${config_dir}"
find "${cert_dir}" -maxdepth 1 -type f -delete
find "${config_dir}" -maxdepth 1 -type f -delete

make_ca() {
  local name="$1"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -sha256 -nodes \
    -days 30 -subj "/CN=FlowSplice ${name}" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -keyout "${cert_dir}/${name}.key" -out "${cert_dir}/${name}.crt" >/dev/null 2>&1
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
    -CA "${cert_dir}/${ca}.crt" -CAkey "${cert_dir}/${ca}.key" -CAcreateserial \
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
issue travel-management travel travel-1 travel-management.flowsplice clientAuth management-ca
issue home-business home home-1 home.flowsplice serverAuth business-ca
issue travel-business travel travel-1 travel.flowsplice clientAuth business-ca

find "${cert_dir}" -maxdepth 1 \( -name '*.csr' -o -name '*.ext' -o -name '*.srl' \) -delete
chmod 600 "${cert_dir}"/*.key
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
travel_management_pin="$(spki_pin travel-management)"
travel_business_pin="$(spki_pin travel-business)"

for template in "${script_dir}"/config/*.toml; do
  output="${config_dir}/$(basename -- "${template}")"
  sed \
    -e "s/__SERVER_PIN__/${server_pin}/g" \
    -e "s/__RELAY1_PIN__/${relay1_pin}/g" \
    -e "s/__RELAY2_PIN__/${relay2_pin}/g" \
    -e "s/__HOME_MANAGEMENT_PIN__/${home_management_pin}/g" \
    -e "s/__HOME_BUSINESS_PIN__/${home_business_pin}/g" \
    -e "s/__TRAVEL_MANAGEMENT_PIN__/${travel_management_pin}/g" \
    -e "s/__TRAVEL_BUSINESS_PIN__/${travel_business_pin}/g" \
    "${template}" >"${output}"
done
printf 'Generated E2E certificates in %s\n' "${cert_dir}"
